//! Offline eval runner — Tier-1 deterministic scorers (PoC build steps 6–7).
//!
//! The offline dataset/experiment regression loop — the core of evald. `evald
//! eval run` loads a JSONL dataset, runs a FIXED set of zero-cost deterministic
//! evaluators (no network, no LLM, no user code) over each item, persists per-item and
//! aggregate results as [`Score`]s, prints a report, and exits non-zero when a
//! configured threshold regresses — a CI gate. `evald eval compare` then reads two runs'
//! persisted aggregate Scores back and diffs them by evaluator, with an optional
//! `--fail-on-regression` run-vs-run gate.
//!
//! Tier-1 evaluators shipped here (all deterministic, zero-cost, no network, no user code):
//! `exact_match`, `contains`, `contains_all`, `contains_any`, `regex`, `json_valid`,
//! `json_schema`, `non_empty`, `length_bounds`, `levenshtein`, `numeric_tolerance`,
//! `equals_numeric`, and the span-derived `latency`/`cost` gates. **Tier-3 LLM-as-judge**
//! (BYO-key, feature-gated) lives in [`crate::judge`] and reuses [`finalize_evaluator`] so its
//! scores share this aggregation + significance path.
//!
//! The span-derived `latency`/`cost` gates read `latency_ms`/`duration_ns` and `cost_usd`
//! from the dataset item's `metadata` (the span attributes materialized into the JSONL row),
//! so every evaluator stays a pure, offline function of the item — no store or network
//! dependency, and it flows through the same aggregation/threshold/`eval compare` path.
//!
//! Scores are OTel-native: a per-item result attaches to the item's `span_id`/`trace_id`
//! when present (so it shows up on the trace), and each evaluator's aggregate is a Score
//! targeting the run (`ScoreTarget::Run`), which is what `eval compare` (step 7) reads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::AggStats;
use crate::stats::{welch_t_test, WelchResult};
use crate::{DataType, Score, ScoreSource, ScoreTarget, Store, StoreConfig};

/// One dataset row (JSONL). `output` is the actual text to score; `expected_output` is
/// the reference for comparison evaluators; `span_id`/`trace_id` (optional) say where to
/// attach the per-item score so it surfaces on the trace.
#[derive(Debug, Clone, Deserialize)]
pub struct DatasetItem {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub input: Option<String>,
    pub output: String,
    #[serde(default)]
    pub expected_output: Option<String>,
    #[serde(default)]
    pub span_id: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
    /// Retrieved context passages (RAG) — the grounding for `faithfulness` / `hallucination`
    /// judge rails. Absent for non-RAG datasets.
    #[serde(default)]
    pub context: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

impl DatasetItem {
    /// Where a per-item score attaches: the span (most specific), else the trace, else
    /// the run.
    fn target(&self, run_id: &str) -> ScoreTarget {
        if let Some(s) = self.span_id.as_deref().filter(|s| !s.is_empty()) {
            ScoreTarget::Span(s.to_string())
        } else if let Some(t) = self.trace_id.as_deref().filter(|t| !t.is_empty()) {
            ScoreTarget::Trace(t.to_string())
        } else {
            ScoreTarget::Run(run_id.to_string())
        }
    }

    /// True if this item points at a concrete span/trace to attach scores to.
    fn has_span_or_trace(&self) -> bool {
        self.span_id.as_deref().is_some_and(|s| !s.is_empty())
            || self.trace_id.as_deref().is_some_and(|t| !t.is_empty())
    }
}

/// Load a JSONL dataset (one [`DatasetItem`] per non-blank line).
pub fn load_dataset(path: &Path) -> anyhow::Result<Vec<DatasetItem>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading dataset {}: {e}", path.display()))?;
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item: DatasetItem = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("dataset {} line {}: {e}", path.display(), i + 1))?;
        items.push(item);
    }
    Ok(items)
}

/// The eval run config (YAML): which dataset, which evaluators, and the pass thresholds.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalConfig {
    #[serde(default)]
    pub name: Option<String>,
    /// Path to the JSONL dataset (relative paths resolve against the config file's dir).
    pub dataset: PathBuf,
    /// Tier-1 deterministic evaluators (default empty so a judges-only config is valid).
    #[serde(default)]
    pub evaluators: Vec<EvaluatorSpec>,
    /// Tier-3 LLM-as-judge evaluators (BYO-key; requires building with `--features judge`).
    #[serde(default)]
    pub judges: Vec<crate::judge::JudgeSpec>,
    /// Per-evaluator minimum mean score for the run to pass (CI gate).
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
}

/// Parse an [`EvalConfig`] from a YAML file.
pub fn load_config(path: &Path) -> anyhow::Result<EvalConfig> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading config {}: {e}", path.display()))?;
    serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing config {}: {e}", path.display()))
}

/// A configured evaluator. The set is FIXED (no user shell/wasm code) — the MVP
/// constraint from PLAN.md §4.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvaluatorSpec {
    /// `output == expected_output` (trimmed).
    ExactMatch,
    /// `output` contains `substring` (or, if unset, `expected_output`).
    Contains {
        #[serde(default)]
        substring: Option<String>,
    },
    /// `output` matches a regular expression.
    Regex { pattern: String },
    /// `output` parses as JSON.
    JsonValid,
    /// Normalized Levenshtein similarity (0..1) between `output` and `expected_output`;
    /// passes when `>= threshold`.
    Levenshtein {
        #[serde(default = "default_levenshtein_threshold")]
        threshold: f64,
    },
    /// `output` is non-empty after trimming whitespace — a cheap "the model produced
    /// something at all" gate.
    NonEmpty,
    /// `output` contains EVERY one of `substrings`. The value is the fraction present
    /// (partial credit, so the mean is informative), and it passes only when all are present.
    ContainsAll { substrings: Vec<String> },
    /// `output` contains AT LEAST ONE of `substrings`.
    ContainsAny { substrings: Vec<String> },
    /// `output`'s character length is within `[min, max]` (each bound optional).
    LengthBounds {
        #[serde(default)]
        min: Option<usize>,
        #[serde(default)]
        max: Option<usize>,
    },
    /// `output` and `expected_output` both parse as numbers and differ by at most
    /// `tolerance` (absolute). Skipped when either side is missing or non-numeric.
    NumericTolerance {
        #[serde(default)]
        tolerance: f64,
    },
    /// `output` and `expected_output` both parse as numbers and are numerically EQUAL —
    /// representation-tolerant exact equality (`"1.0" == "1" == "1e0"`). Skipped when either
    /// side is missing or non-numeric. (Use `numeric_tolerance` for an approximate match.)
    EqualsNumeric,
    /// `output` parses as JSON AND validates against the inline JSON Schema `schema`. Invalid
    /// JSON or a schema violation fails (with the first error in the comment). Fully offline —
    /// only the inline schema is resolved; external `$ref`s are not fetched.
    JsonSchema { schema: serde_json::Value },
    /// Span-derived latency gate: passes when the item's recorded latency is `<= max_ms`.
    /// Reads `latency_ms` (else the span-native `duration_ns`, converted) from the item's
    /// `metadata`. Skipped when neither is present — never silently failed.
    Latency { max_ms: f64 },
    /// Span-derived cost gate: passes when the item's recorded `cost_usd` (from `metadata`)
    /// is `<= max_usd`. Skipped when absent.
    Cost { max_usd: f64 },
}

fn default_levenshtein_threshold() -> f64 {
    0.8
}

impl EvaluatorSpec {
    /// The evaluator's stable name (also its threshold key).
    pub fn name(&self) -> &'static str {
        match self {
            EvaluatorSpec::ExactMatch => "exact_match",
            EvaluatorSpec::Contains { .. } => "contains",
            EvaluatorSpec::Regex { .. } => "regex",
            EvaluatorSpec::JsonValid => "json_valid",
            EvaluatorSpec::Levenshtein { .. } => "levenshtein",
            EvaluatorSpec::NonEmpty => "non_empty",
            EvaluatorSpec::ContainsAll { .. } => "contains_all",
            EvaluatorSpec::ContainsAny { .. } => "contains_any",
            EvaluatorSpec::LengthBounds { .. } => "length_bounds",
            EvaluatorSpec::NumericTolerance { .. } => "numeric_tolerance",
            EvaluatorSpec::EqualsNumeric => "equals_numeric",
            EvaluatorSpec::JsonSchema { .. } => "json_schema",
            EvaluatorSpec::Latency { .. } => "latency",
            EvaluatorSpec::Cost { .. } => "cost",
        }
    }

    /// Compile to a runnable [`Evaluator`], validating params (e.g. the regex). This is the **single
    /// evaluator interface**: the batch runner ([`run_eval`]), the per-item [`Self::evaluate`], and
    /// any experiment path all funnel through the same compiled [`Evaluator::score`], so there is no
    /// batch-only vs per-item scoring divergence (a split batch/per-item interface is a known source
    /// of scoring drift; a single interface avoids it by construction).
    pub fn compile(&self) -> anyhow::Result<Box<dyn Evaluator>> {
        Ok(match self {
            EvaluatorSpec::ExactMatch => Box::new(ExactMatch),
            EvaluatorSpec::Contains { substring } => Box::new(Contains {
                substring: substring.clone(),
            }),
            EvaluatorSpec::Regex { pattern } => Box::new(RegexEval {
                re: regex::Regex::new(pattern)
                    .map_err(|e| anyhow::anyhow!("invalid regex {pattern:?}: {e}"))?,
            }),
            EvaluatorSpec::JsonValid => Box::new(JsonValid),
            EvaluatorSpec::Levenshtein { threshold } => Box::new(Levenshtein {
                threshold: *threshold,
            }),
            EvaluatorSpec::NonEmpty => Box::new(NonEmpty),
            EvaluatorSpec::ContainsAll { substrings } => {
                anyhow::ensure!(
                    !substrings.is_empty(),
                    "contains_all needs a non-empty `substrings` list"
                );
                Box::new(ContainsAll {
                    substrings: substrings.clone(),
                })
            }
            EvaluatorSpec::ContainsAny { substrings } => {
                anyhow::ensure!(
                    !substrings.is_empty(),
                    "contains_any needs a non-empty `substrings` list"
                );
                Box::new(ContainsAny {
                    substrings: substrings.clone(),
                })
            }
            EvaluatorSpec::LengthBounds { min, max } => {
                if let (Some(lo), Some(hi)) = (min, max) {
                    anyhow::ensure!(lo <= hi, "length_bounds: min ({lo}) must be <= max ({hi})");
                }
                anyhow::ensure!(
                    min.is_some() || max.is_some(),
                    "length_bounds needs at least one of `min`/`max`"
                );
                Box::new(LengthBounds {
                    min: *min,
                    max: *max,
                })
            }
            EvaluatorSpec::NumericTolerance { tolerance } => {
                anyhow::ensure!(
                    tolerance.is_finite() && *tolerance >= 0.0,
                    "numeric_tolerance must be a finite value >= 0.0"
                );
                Box::new(NumericTolerance {
                    tolerance: *tolerance,
                })
            }
            EvaluatorSpec::EqualsNumeric => Box::new(EqualsNumeric),
            EvaluatorSpec::JsonSchema { schema } => {
                let validator = jsonschema::validator_for(schema)
                    .map_err(|e| anyhow::anyhow!("invalid json_schema: {e}"))?;
                Box::new(JsonSchemaEval { validator })
            }
            EvaluatorSpec::Latency { max_ms } => {
                anyhow::ensure!(
                    max_ms.is_finite() && *max_ms >= 0.0,
                    "latency: max_ms must be a finite value >= 0.0"
                );
                Box::new(Latency { max_ms: *max_ms })
            }
            EvaluatorSpec::Cost { max_usd } => {
                anyhow::ensure!(
                    max_usd.is_finite() && *max_usd >= 0.0,
                    "cost: max_usd must be a finite value >= 0.0"
                );
                Box::new(Cost { max_usd: *max_usd })
            }
        })
    }

    /// Score one item through the same compiled evaluator the batch runner uses — the per-item entry
    /// point of the single evaluator interface. `Ok(None)` = the item lacks what this evaluator needs
    /// (skipped, not failed). Compiles the spec each call (validating params), so prefer
    /// [`Self::compile`] once + [`Evaluator::score`] in a hot loop.
    pub fn evaluate(&self, item: &DatasetItem) -> anyhow::Result<Option<ItemScore>> {
        Ok(self.compile()?.score(item))
    }
}

/// Per-item evaluation outcome (`value` in 0..1).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemScore {
    pub value: f64,
    pub passed: bool,
    pub comment: Option<String>,
}

/// A deterministic, zero-cost scorer over a dataset item.
pub trait Evaluator {
    fn name(&self) -> &str;
    /// Score the item, or `None` when the item lacks what this evaluator needs (e.g. no
    /// `expected_output` for `exact_match`) — counted as skipped, not failed.
    fn score(&self, item: &DatasetItem) -> Option<ItemScore>;
}

struct ExactMatch;
impl Evaluator for ExactMatch {
    fn name(&self) -> &str {
        "exact_match"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected = item.expected_output.as_deref()?;
        let pass = item.output.trim() == expected.trim();
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct Contains {
    substring: Option<String>,
}
impl Evaluator for Contains {
    fn name(&self) -> &str {
        "contains"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let needle = self
            .substring
            .as_deref()
            .or(item.expected_output.as_deref())?;
        let pass = item.output.contains(needle);
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct RegexEval {
    re: regex::Regex,
}
impl Evaluator for RegexEval {
    fn name(&self) -> &str {
        "regex"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let pass = self.re.is_match(&item.output);
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct JsonValid;
impl Evaluator for JsonValid {
    fn name(&self) -> &str {
        "json_valid"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let pass = serde_json::from_str::<serde_json::Value>(&item.output).is_ok();
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: if pass {
                None
            } else {
                Some("output is not valid JSON".to_string())
            },
        })
    }
}

struct Levenshtein {
    threshold: f64,
}
impl Evaluator for Levenshtein {
    fn name(&self) -> &str {
        "levenshtein"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected = item.expected_output.as_deref()?;
        let value = levenshtein_similarity(&item.output, expected);
        Some(ItemScore {
            passed: value >= self.threshold,
            value,
            comment: None,
        })
    }
}

struct NonEmpty;
impl Evaluator for NonEmpty {
    fn name(&self) -> &str {
        "non_empty"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let pass = !item.output.trim().is_empty();
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct ContainsAll {
    substrings: Vec<String>,
}
impl Evaluator for ContainsAll {
    fn name(&self) -> &str {
        "contains_all"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let present = self
            .substrings
            .iter()
            .filter(|s| item.output.contains(s.as_str()))
            .count();
        let value = present as f64 / self.substrings.len() as f64;
        let passed = present == self.substrings.len();
        let comment = (!passed).then(|| {
            let missing: Vec<&str> = self
                .substrings
                .iter()
                .filter(|s| !item.output.contains(s.as_str()))
                .map(String::as_str)
                .collect();
            format!("missing: {}", missing.join(", "))
        });
        Some(ItemScore {
            value,
            passed,
            comment,
        })
    }
}

struct ContainsAny {
    substrings: Vec<String>,
}
impl Evaluator for ContainsAny {
    fn name(&self) -> &str {
        "contains_any"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let pass = self
            .substrings
            .iter()
            .any(|s| item.output.contains(s.as_str()));
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct LengthBounds {
    min: Option<usize>,
    max: Option<usize>,
}
impl Evaluator for LengthBounds {
    fn name(&self) -> &str {
        "length_bounds"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let len = item.output.chars().count();
        let pass = self.min.is_none_or(|lo| len >= lo) && self.max.is_none_or(|hi| len <= hi);
        let comment = (!pass).then(|| {
            format!(
                "length {len} outside bounds (min={:?}, max={:?})",
                self.min, self.max
            )
        });
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment,
        })
    }
}

struct NumericTolerance {
    tolerance: f64,
}
impl Evaluator for NumericTolerance {
    fn name(&self) -> &str {
        "numeric_tolerance"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        // Skipped (None) unless both sides are present and parse as numbers — a non-numeric
        // output is "this evaluator doesn't apply", not a failure.
        let expected = item.expected_output.as_deref()?;
        let a: f64 = item.output.trim().parse().ok()?;
        let b: f64 = expected.trim().parse().ok()?;
        let pass = (a - b).abs() <= self.tolerance;
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct EqualsNumeric;
impl Evaluator for EqualsNumeric {
    fn name(&self) -> &str {
        "equals_numeric"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        // Skipped (None) unless both sides parse as numbers — a non-numeric side means "this
        // evaluator doesn't apply", not a failure (mirrors `numeric_tolerance`).
        let expected = item.expected_output.as_deref()?;
        let a: f64 = item.output.trim().parse().ok()?;
        let b: f64 = expected.trim().parse().ok()?;
        // Equality on independently-parsed values: representation differences ("1.0"/"1")
        // collapse to the same f64, so this is exact yet format-tolerant. NaN is handled
        // explicitly — IEEE-754 makes `NaN == NaN` false, but two outputs that both parse to
        // NaN should match (else "NaN" vs "nan" is silently FAILed, not treated as equal).
        let pass = a == b || (a.is_nan() && b.is_nan());
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: None,
        })
    }
}

struct JsonSchemaEval {
    validator: jsonschema::Validator,
}
impl Evaluator for JsonSchemaEval {
    fn name(&self) -> &str {
        "json_schema"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        // Non-JSON output can't satisfy a schema — that's a FAIL (like `json_valid`), not a
        // skip: a schema gate that silently ignored garbage output would be worthless.
        let instance: serde_json::Value = match serde_json::from_str(&item.output) {
            Ok(v) => v,
            Err(_) => {
                return Some(ItemScore {
                    value: 0.0,
                    passed: false,
                    comment: Some("output is not valid JSON".to_string()),
                })
            }
        };
        // `is_valid` is the fast boolean path; only re-run `validate` (for the first error
        // message) on the failing minority.
        let pass = self.validator.is_valid(&instance);
        let comment = (!pass).then(|| match self.validator.validate(&instance) {
            Err(e) => format!("schema violation: {e}"),
            Ok(()) => "schema violation".to_string(),
        });
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment,
        })
    }
}

struct Latency {
    max_ms: f64,
}
impl Evaluator for Latency {
    fn name(&self) -> &str {
        "latency"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        // Prefer an explicit `latency_ms`; else derive from the span-native `duration_ns`.
        let ms = metadata_f64(item, "latency_ms")
            .or_else(|| metadata_f64(item, "duration_ns").map(|ns| ns / 1_000_000.0))?;
        let pass = ms <= self.max_ms;
        let comment =
            (!pass).then(|| format!("latency {ms:.3}ms exceeds max_ms {:.3}", self.max_ms));
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment,
        })
    }
}

struct Cost {
    max_usd: f64,
}
impl Evaluator for Cost {
    fn name(&self) -> &str {
        "cost"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let usd = metadata_f64(item, "cost_usd")?;
        let pass = usd <= self.max_usd;
        let comment =
            (!pass).then(|| format!("cost ${usd:.6} exceeds max_usd ${:.6}", self.max_usd));
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment,
        })
    }
}

/// Read a numeric field from an item's `metadata` object (span attributes materialized into
/// the dataset row). Returns `None` when metadata is absent, not an object, the key is
/// missing, or the value isn't a finite number — so a span-derived gate SKIPS rather than
/// fabricating a pass/fail from missing data. Accepts a JSON number or a numeric string
/// (some exporters stringify attribute values).
fn metadata_f64(item: &DatasetItem, key: &str) -> Option<f64> {
    let v = item.metadata.as_ref()?.get(key)?;
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
    n.is_finite().then_some(n)
}

/// Normalized Levenshtein similarity in 0..1 (`1 - edits / max_len`).
fn levenshtein_similarity(a: &str, b: &str) -> f64 {
    let max = a.chars().count().max(b.chars().count());
    if max == 0 {
        return 1.0;
    }
    1.0 - (levenshtein(a, b) as f64 / max as f64)
}

/// Levenshtein edit distance (two-row DP over `char`s).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Per-evaluator aggregate over the dataset.
#[derive(Debug, Clone)]
pub struct EvaluatorAggregate {
    pub name: String,
    pub mean: f64,
    pub pass_rate: f64,
    pub scored: usize,
    pub skipped: usize,
    pub threshold: Option<f64>,
    pub threshold_met: bool,
}

/// The result of an eval run.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub run_id: String,
    pub name: Option<String>,
    pub item_count: usize,
    pub aggregates: Vec<EvaluatorAggregate>,
}

impl RunReport {
    /// True iff every configured threshold was met (the CI gate passes).
    pub fn passed(&self) -> bool {
        self.aggregates.iter().all(|a| a.threshold_met)
    }
}

/// Run the evaluators over the dataset. Returns the report plus the [`Score`]s to
/// persist: a per-item score for each item that points at a span/trace, and one
/// aggregate score per evaluator targeting the run.
pub fn run_eval(
    config: &EvalConfig,
    dataset: &[DatasetItem],
    run_id: &str,
    ts_unix_nano: u64,
) -> anyhow::Result<(RunReport, Vec<Score>)> {
    let evaluators: Vec<Box<dyn Evaluator>> = config
        .evaluators
        .iter()
        .map(|spec| spec.compile())
        .collect::<anyhow::Result<_>>()?;

    // Scores, thresholds, and the `eval compare` aggregate map are all keyed by the
    // evaluator's name, so two evaluators of the same kind (e.g. two `regex` with
    // different patterns) would silently clobber each other. Reject duplicates up front.
    let mut seen = BTreeSet::new();
    for ev in &evaluators {
        anyhow::ensure!(
            seen.insert(ev.name()),
            "duplicate evaluator {:?}: each evaluator kind may appear at most once \
             (scores and thresholds are keyed by evaluator name)",
            ev.name()
        );
    }

    let mut scores = Vec::new();
    let mut aggregates = Vec::new();

    for ev in &evaluators {
        let outcomes: Vec<Option<ItemScore>> = dataset.iter().map(|item| ev.score(item)).collect();
        let threshold = config.thresholds.get(ev.name()).copied();
        let agg = finalize_evaluator(
            ev.name(),
            outcomes,
            dataset,
            run_id,
            threshold,
            ts_unix_nano,
            &mut scores,
        );
        aggregates.push(agg);
    }

    Ok((
        RunReport {
            run_id: run_id.to_string(),
            name: config.name.clone(),
            item_count: dataset.len(),
            aggregates,
        },
        scores,
    ))
}

/// Turn one evaluator's per-item outcomes (`outcomes[i]` aligns to `dataset[i]`; `None` =
/// skipped) into its persisted Scores + aggregate. Shared by Tier-1 deterministic scorers and
/// Tier-3 judges so both flow through the SAME per-item persistence, sufficient-statistics
/// (n / pass_count / mean / variance), threshold gate, and `eval compare` significance path.
///
/// Variance is a stable two-pass about the mean (exactly 0.0 for a constant set, so a
/// constant-vs-constant `eval compare` hits Welch's deterministic branch, not an absurd t).
pub(crate) fn finalize_evaluator(
    name: &str,
    outcomes: Vec<Option<ItemScore>>,
    dataset: &[DatasetItem],
    run_id: &str,
    threshold: Option<f64>,
    ts_unix_nano: u64,
    scores: &mut Vec<Score>,
) -> EvaluatorAggregate {
    let mut values: Vec<f64> = Vec::new();
    let mut passes = 0usize;

    for (idx, (item, outcome)) in dataset.iter().zip(outcomes).enumerate() {
        let Some(outcome) = outcome else {
            continue; // skipped — missing required field, judge parse failure, etc.
        };
        values.push(outcome.value);
        if outcome.passed {
            passes += 1;
        }
        // Persist the per-item score only when it can attach to a span/trace.
        if item.has_span_or_trace() {
            scores.push(Score {
                id: format!("{run_id}:{name}:{idx}"),
                target: item.target(run_id),
                name: name.to_string(),
                num_value: Some(outcome.value),
                str_value: None,
                data_type: DataType::Numeric,
                source: ScoreSource::Eval,
                comment: outcome.comment,
                config_id: Some(run_id.to_string()),
                agg_stats: None, // per-item scores carry no run-level aggregate
                ts_unix_nano,
            });
        }
    }

    let scored = values.len();
    let skipped = dataset.len() - scored;
    let mean = if scored > 0 {
        values.iter().sum::<f64>() / scored as f64
    } else {
        0.0
    };
    let variance = if scored > 1 {
        let n = scored as f64;
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    let pass_rate = if scored > 0 {
        passes as f64 / scored as f64
    } else {
        0.0
    };
    // An evaluator that scored nothing can't have regressed — don't fail the CI gate on a
    // fabricated 0.0 mean; it surfaces as fully skipped in the report instead.
    let threshold_met = scored == 0 || threshold.is_none_or(|t| mean >= t);

    // The aggregate IS a Score targeting the run — what `eval compare` reads back, with the
    // sufficient statistics for the Welch's-t significance test.
    scores.push(Score {
        id: format!("{run_id}:agg:{name}"),
        target: ScoreTarget::Run(run_id.to_string()),
        name: name.to_string(),
        num_value: Some(mean),
        str_value: None,
        data_type: DataType::Numeric,
        source: ScoreSource::Eval,
        comment: Some(format!(
            "aggregate mean over {scored} item(s); pass_rate {pass_rate:.3}"
        )),
        config_id: Some(run_id.to_string()),
        agg_stats: Some(AggStats {
            n: scored as u64,
            pass_count: passes as u64,
            mean,
            variance,
        }),
        ts_unix_nano,
    });

    EvaluatorAggregate {
        name: name.to_string(),
        mean,
        pass_rate,
        scored,
        skipped,
        threshold,
        threshold_met,
    }
}

/// `evald eval run` — load the config + dataset, evaluate, persist scores, print a
/// report, and return whether every threshold was met (the caller maps `false` to a
/// non-zero exit code for CI).
pub async fn run_command(config_path: &Path, data_dir: &Path) -> anyhow::Result<bool> {
    let config = load_config(config_path)?;
    let dataset_path = resolve_relative(config_path, &config.dataset);
    let dataset = load_dataset(&dataset_path)?;
    anyhow::ensure!(
        !dataset.is_empty(),
        "dataset {} has no items",
        dataset_path.display()
    );

    // Every Tier-1 evaluator name + judge name shares one space (scores + thresholds key by
    // name). Reject collisions UP FRONT — before any judge makes a paid LLM call.
    {
        let mut seen = BTreeSet::new();
        for name in config
            .evaluators
            .iter()
            .map(|e| e.name().to_string())
            .chain(config.judges.iter().map(|j| j.name()))
        {
            anyhow::ensure!(
                seen.insert(name.clone()),
                "duplicate evaluator name {name:?} in config (Tier-1 evaluators and judges share one name space)"
            );
        }
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    let ts = now_unix_nano();
    let (mut report, mut scores) = run_eval(&config, &dataset, &run_id, ts)?;

    // Tier-3 judges (BYO-key, off by default) append to the SAME report + scores, so they flow
    // through the threshold gate, persistence, and `eval compare` significance like Tier-1.
    if !config.judges.is_empty() {
        let judge_aggs = crate::judge::run_all_judges(
            &config.judges,
            &dataset,
            &run_id,
            ts,
            data_dir,
            &config.thresholds,
            &mut scores,
        )
        .await?;
        report.aggregates.extend(judge_aggs);
    }

    // Open the store embedded (no background compactor for a short-lived CLI) and persist.
    // Note: this takes the data-dir's redb lock, so it can't run against a live `serve`
    // on the same dir — the CI use case runs standalone.
    let store_config = StoreConfig {
        compact_interval: None,
        ..StoreConfig::default()
    };
    let store = Store::open(data_dir, store_config)
        .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    store
        .put_scores(&scores)
        .map_err(|e| anyhow::anyhow!("persisting scores: {e}"))?;

    print_report(&report, &dataset_path);
    Ok(report.passed())
}

/// `evald eval run --estimate` — preview the judge token usage + cost for this config WITHOUT
/// calling any provider (Tier-1 scorers are zero-cost, so only judges are estimated).
pub fn estimate_command(config_path: &Path, data_dir: &Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let dataset_path = resolve_relative(config_path, &config.dataset);
    let dataset = load_dataset(&dataset_path)?;
    if config.judges.is_empty() {
        println!("No judges configured — Tier-1 evaluators are zero-cost, nothing to estimate.");
        return Ok(());
    }
    let estimates = crate::judge::estimate_judges(&config.judges, &dataset, data_dir)?;
    print_estimate(&estimates, dataset.len());
    Ok(())
}

/// Print the judge cost estimate (indicative — see the disclaimer line).
fn print_estimate(estimates: &[crate::judge::JudgeEstimate], item_count: usize) {
    println!("\nevald eval — judge cost estimate ({item_count} item(s))");
    println!(
        "{:<22} {:>8} {:>8} {:>12} {:>12} {:>12}",
        "judge", "to_call", "cached", "in_tokens", "out_tokens", "est_cost"
    );
    let (mut total, mut any_cost, mut any_unknown) = (0.0f64, false, false);
    for e in estimates {
        let cost = match e.cost_usd {
            Some(c) => {
                total += c;
                any_cost = true;
                format!("${c:.4}")
            }
            None => {
                any_unknown = true;
                "?".to_string()
            }
        };
        println!(
            "{:<22} {:>8} {:>8} {:>12} {:>12} {:>12}",
            e.name, e.calls, e.cached, e.input_tokens, e.output_tokens, cost
        );
    }
    if any_cost {
        let tail = if any_unknown {
            " (+ unknown-priced models marked ?)"
        } else {
            ""
        };
        println!("\nestimated total: ${total:.4}{tail}");
    } else {
        println!(
            "\nno priced model recognized — token estimates shown; check your provider's rates."
        );
    }
    println!(
        "note: INDICATIVE only — input tokens are ~chars/4, output is assumed ~32 tokens/call, \
         and prices drift. Cached items cost $0."
    );
}

/// Resolve `dataset` relative to the config file's directory (absolute paths unchanged).
pub(crate) fn resolve_relative(config_path: &Path, dataset: &Path) -> PathBuf {
    if dataset.is_absolute() {
        return dataset.to_path_buf();
    }
    match config_path.parent() {
        Some(dir) => dir.join(dataset),
        None => dataset.to_path_buf(),
    }
}

/// Print the eval report to stdout (user-facing, not via the tracing log).
fn print_report(report: &RunReport, dataset_path: &Path) {
    println!(
        "\nevald eval — {} ({} item(s) from {})",
        report.name.as_deref().unwrap_or("unnamed"),
        report.item_count,
        dataset_path.display()
    );
    println!("run_id: {}", report.run_id);
    println!(
        "{:<22} {:>7} {:>10} {:>8} {:>10} {:>7}",
        "evaluator", "mean", "pass_rate", "scored", "threshold", "result"
    );
    for a in &report.aggregates {
        let threshold = a
            .threshold
            .map(|t| format!("{t:.3}"))
            .unwrap_or_else(|| "-".to_string());
        let result = if a.scored == 0 {
            "skip" // nothing scored — not a pass and not a regression
        } else if !a.threshold_met {
            "FAIL"
        } else if a.threshold.is_some() {
            "pass"
        } else {
            "-"
        };
        let skipped = if a.skipped > 0 {
            format!(" ({} skipped)", a.skipped)
        } else {
            String::new()
        };
        println!(
            "{:<22} {:>7.3} {:>10.3} {:>8} {:>10} {:>7}{}",
            a.name, a.mean, a.pass_rate, a.scored, threshold, result, skipped
        );
    }
    if report.passed() {
        println!("\nEVAL PASSED — all thresholds met.");
    } else {
        let n = report
            .aggregates
            .iter()
            .filter(|a| !a.threshold_met)
            .count();
        println!("\nEVAL FAILED — {n} threshold(s) regressed.");
    }
}

// ---------------------------------------------------------------------------
// `evald eval compare` — run-vs-run aggregate diff (PoC build step 7).
//
// Reads back the run-targeted aggregate Scores that `eval run` persisted (one per
// evaluator, id `{run_id}:agg:{name}`, targeting `ScoreTarget::Run`) for two runs and
// diffs their means by evaluator name. `--fail-on-regression` turns it into a CI gate:
// a regression is a metric that dropped by more than `--tolerance`. Every Tier-1 scorer
// is higher-is-better in `[0, 1]`, so a negative delta ("DOWN") means run B got worse.
// ---------------------------------------------------------------------------

/// One evaluator's mean in run A vs run B. `a`/`b` are `None` when the evaluator only ran
/// on the other side; `delta` (`b - a`) is `None` unless both sides are present. `signif`
/// holds the Welch's-t significance verdict (p-value + CI on `delta`) when **both** sides
/// carried sufficient statistics (n ≥ 2); it is `None` for an evaluator present on only one
/// side, for tiny samples, or for runs persisted before stats were recorded (back-compat).
#[derive(Debug, Clone)]
pub struct CompareRow {
    pub evaluator: String,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub delta: Option<f64>,
    /// Sample sizes (scored items) per side, when known.
    pub n_a: Option<u64>,
    pub n_b: Option<u64>,
    /// Welch's-t result: the p-value and the `(1 − alpha)` CI on `delta`.
    pub signif: Option<WelchResult>,
}

impl CompareRow {
    /// A numeric regression: present on both sides and B is worse than A by strictly more
    /// than `tolerance` (higher-is-better scorers). Evaluators missing on one side are
    /// *not* counted — the evaluator sets can legitimately differ across runs — but they
    /// are still surfaced in the table as `new`/`gone`.
    fn is_regression(&self, tolerance: f64) -> bool {
        matches!(self.delta, Some(d) if d < -tolerance)
    }

    /// We can statistically *prove* this regression is within sampling noise: the test ran
    /// and the `(1 − alpha)` CI on `delta` still includes 0 (≥ 0 upper bound) — i.e. we
    /// can't rule out "no change". An untestable row (no stats) is NOT proven noise.
    fn proven_noise(&self) -> bool {
        matches!(self.signif, Some(s) if s.ci_high >= 0.0)
    }

    /// A *statistically significant* regression: a regression beyond `tolerance` whose
    /// `(1 − alpha)` CI lies entirely below 0 (`ci_high < 0`). Requires the test to have run.
    fn is_significant_regression(&self, tolerance: f64) -> bool {
        self.is_regression(tolerance) && matches!(self.signif, Some(s) if s.ci_high < 0.0)
    }

    /// Whether this row fails the **significance-mode** gate: a regression beyond `tolerance`
    /// that we could NOT prove is noise. We forgive a drop only when the test shows it is
    /// within noise; an untestable regression (one side lacks stats) still gates, so a real
    /// regression never slips through just because an old run predates the stats field.
    fn gates_in_significance_mode(&self, tolerance: f64) -> bool {
        self.is_regression(tolerance) && !self.proven_noise()
    }
}

/// A run-vs-run aggregate comparison: one [`CompareRow`] per evaluator seen in either run.
#[derive(Debug, Clone)]
pub struct CompareReport {
    pub run_a: String,
    pub run_b: String,
    pub rows: Vec<CompareRow>,
}

impl CompareReport {
    /// True if any shared evaluator regressed beyond `tolerance` — the default CI gate when
    /// `--fail-on-regression` is set (raw-delta, no significance test).
    pub fn has_regression(&self, tolerance: f64) -> bool {
        self.rows.iter().any(|r| r.is_regression(tolerance))
    }

    /// True if any beyond-`tolerance` regression is not provably noise — the significance-mode
    /// gate (`--fail-on-regression --significance`).
    pub fn has_gating_significant_regression(&self, tolerance: f64) -> bool {
        self.rows
            .iter()
            .any(|r| r.gates_in_significance_mode(tolerance))
    }
}

/// One run's per-evaluator aggregate: the mean (always) plus the sufficient statistics when
/// the run was produced by a stats-aware `eval run`.
#[derive(Debug, Clone, Copy)]
struct AggEntry {
    mean: f64,
    stats: Option<AggStats>,
}

/// Read back a run's per-evaluator aggregates: the Run-targeted Scores whose id marks them as
/// aggregates (`{run_id}:agg:{name}`), keyed by evaluator name. Carries `agg_stats` forward
/// when present so the caller can run a significance test. Only aggregates target a run today,
/// but the id-prefix filter keeps this correct if that changes.
fn run_aggregates(store: &Store, run_id: &str) -> anyhow::Result<BTreeMap<String, AggEntry>> {
    let prefix = format!("{run_id}:agg:");
    let scores = store
        .scores_for_target(&ScoreTarget::Run(run_id.to_string()))
        .map_err(|e| anyhow::anyhow!("reading scores for run {run_id}: {e}"))?;
    let mut out = BTreeMap::new();
    for s in scores {
        if !s.id.starts_with(&prefix) {
            continue;
        }
        if let Some(v) = s.num_value {
            out.insert(
                s.name,
                AggEntry {
                    mean: v,
                    stats: s.agg_stats,
                },
            );
        }
    }
    Ok(out)
}

/// Diff two runs' aggregates by evaluator name, attaching a Welch's-t significance verdict
/// (p-value + `(1 − alpha)` CI on the delta) wherever both runs carried sufficient statistics.
/// Bails if either run has no aggregate scores (an unknown id, or a run whose scores were
/// never persisted).
pub fn compare_runs(
    store: &Store,
    run_a: &str,
    run_b: &str,
    alpha: f64,
) -> anyhow::Result<CompareReport> {
    let agg_a = run_aggregates(store, run_a)?;
    let agg_b = run_aggregates(store, run_b)?;
    anyhow::ensure!(
        !agg_a.is_empty(),
        "no aggregate scores found for run {run_a} — unknown run id, or its scores were never persisted"
    );
    anyhow::ensure!(
        !agg_b.is_empty(),
        "no aggregate scores found for run {run_b} — unknown run id, or its scores were never persisted"
    );

    let names: BTreeSet<&String> = agg_a.keys().chain(agg_b.keys()).collect();
    let rows = names
        .into_iter()
        .map(|name| {
            let ea = agg_a.get(name);
            let eb = agg_b.get(name);
            let a = ea.map(|e| e.mean);
            let b = eb.map(|e| e.mean);
            let delta = match (a, b) {
                (Some(a), Some(b)) => Some(b - a),
                _ => None,
            };
            let n_a = ea.and_then(|e| e.stats).map(|s| s.n);
            let n_b = eb.and_then(|e| e.stats).map(|s| s.n);
            let signif = match (ea.and_then(|e| e.stats), eb.and_then(|e| e.stats)) {
                (Some(sa), Some(sb)) => welch_t_test(
                    sa.mean,
                    sa.variance,
                    sa.n,
                    sb.mean,
                    sb.variance,
                    sb.n,
                    alpha,
                ),
                _ => None,
            };
            CompareRow {
                evaluator: name.clone(),
                a,
                b,
                delta,
                n_a,
                n_b,
                signif,
            }
        })
        .collect();

    Ok(CompareReport {
        run_a: run_a.to_string(),
        run_b: run_b.to_string(),
        rows,
    })
}

/// `evald eval compare` — open the store, diff the two runs' aggregates, print the table, and
/// return whether the gate passes. With `fail_on_regression`, returns `false` (→ the caller
/// exits non-zero) when a regression beyond `tolerance` is seen; with `significance` on, only
/// a regression that is **not provably within noise** at level `alpha` gates. Without
/// `fail_on_regression`, always returns `true`.
pub async fn compare_command(
    run_a: &str,
    run_b: &str,
    data_dir: &Path,
    fail_on_regression: bool,
    tolerance: f64,
    significance: bool,
    alpha: f64,
) -> anyhow::Result<bool> {
    let store_config = StoreConfig {
        compact_interval: None,
        ..StoreConfig::default()
    };
    let store = Store::open(data_dir, store_config)
        .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let report = compare_runs(&store, run_a, run_b, alpha)?;
    print_compare(&report, tolerance, significance, alpha);

    let regressed = if significance {
        report.has_gating_significant_regression(tolerance)
    } else {
        report.has_regression(tolerance)
    };
    let gate_fails = fail_on_regression && regressed;
    if gate_fails {
        println!("--fail-on-regression set — exiting non-zero.");
    }
    Ok(!gate_fails)
}

/// Print the run-vs-run diff to stdout (the report; logs go to stderr so CI can parse it).
/// Whenever a row carries a significance test, the p-value, the `(1 − alpha)` CI on the delta,
/// and a `signif`/`noise` verdict are shown alongside the raw delta.
fn print_compare(report: &CompareReport, tolerance: f64, significance: bool, alpha: f64) {
    let cell = |v: Option<f64>| {
        v.map(|x| format!("{x:.3}"))
            .unwrap_or_else(|| "-".to_string())
    };
    let conf = (1.0 - alpha) * 100.0;
    let any_stats = report.rows.iter().any(|r| r.signif.is_some());

    println!("\nevald eval compare");
    println!("run A: {}", report.run_a);
    println!("run B: {}", report.run_b);
    if any_stats {
        println!(
            "{:<14} {:>9} {:>9} {:>9}  {:<6} {:>8} {:>19} {:>7}",
            "evaluator",
            "run_a",
            "run_b",
            "delta",
            "change",
            "p-value",
            format!("{conf:.0}% CI Δ"),
            "verdict"
        );
    } else {
        println!(
            "{:<14} {:>9} {:>9} {:>9}  change",
            "evaluator", "run_a", "run_b", "delta"
        );
    }

    for r in &report.rows {
        let (delta, change) = match r.delta {
            Some(d) if d < -tolerance => (format!("{d:+.3}"), "DOWN"),
            Some(d) if d > tolerance => (format!("{d:+.3}"), "up"),
            Some(d) => (format!("{d:+.3}"), "same"),
            None if r.a.is_none() => ("-".to_string(), "new"), // only in run B
            None => ("-".to_string(), "gone"),                 // only in run A
        };
        if any_stats {
            let (pval, ci, verdict) = match r.signif {
                Some(s) => {
                    let significant = s.ci_high < 0.0 || s.ci_low > 0.0;
                    (
                        format!("{:.4}", s.p_two_sided),
                        format!("[{:+.3},{:+.3}]", s.ci_low, s.ci_high),
                        if significant { "signif" } else { "noise" },
                    )
                }
                // Both-present-but-untestable (n<2) vs only-one-side both land here as n/a.
                None => ("-".to_string(), "-".to_string(), "n/a"),
            };
            println!(
                "{:<14} {:>9} {:>9} {:>9}  {:<6} {:>8} {:>19} {:>7}",
                r.evaluator,
                cell(r.a),
                cell(r.b),
                delta,
                change,
                pval,
                ci,
                verdict
            );
        } else {
            println!(
                "{:<14} {:>9} {:>9} {:>9}  {}",
                r.evaluator,
                cell(r.a),
                cell(r.b),
                delta,
                change
            );
        }
    }

    // Footer reflects the ACTIVE gate (significance vs raw-delta).
    if significance {
        let gating = report
            .rows
            .iter()
            .filter(|r| r.gates_in_significance_mode(tolerance))
            .count();
        let within_noise = report
            .rows
            .iter()
            .filter(|r| r.is_regression(tolerance) && r.proven_noise())
            .count();
        if gating > 0 {
            println!(
                "\nSIGNIFICANT REGRESSION — {gating} evaluator(s) dropped > {tolerance:.3} and are \
                 not within {conf:.0}% sampling noise (or could not be tested)."
            );
        } else if within_noise > 0 {
            println!(
                "\nNo significant regression — {within_noise} drop(s) beyond {tolerance:.3} are \
                 within {conf:.0}% sampling noise."
            );
        } else {
            println!("\nNo regression — every shared evaluator held within {tolerance:.3}.");
        }
    } else if report.has_regression(tolerance) {
        let n = report
            .rows
            .iter()
            .filter(|r| r.is_regression(tolerance))
            .count();
        // Stated as a neutral fact — whether this is a CI failure depends on
        // `--fail-on-regression`, which `compare_command` (not this printer) decides.
        let sig = report
            .rows
            .iter()
            .filter(|r| r.is_significant_regression(tolerance))
            .count();
        let hint = if any_stats {
            format!(" ({sig} statistically significant at {conf:.0}% — see --significance)")
        } else {
            String::new()
        };
        println!(
            "\nREGRESSION — {n} evaluator(s) dropped > {tolerance:.3} from run A to run B.{hint}"
        );
    } else {
        println!("\nNo regression — every shared evaluator held within {tolerance:.3}.");
    }
}

fn now_unix_nano() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(output: &str, expected: Option<&str>) -> DatasetItem {
        DatasetItem {
            id: None,
            input: None,
            output: output.to_string(),
            expected_output: expected.map(String::from),
            span_id: None,
            trace_id: None,
            context: None,
            metadata: None,
        }
    }

    fn meta_item(metadata: serde_json::Value) -> DatasetItem {
        DatasetItem {
            metadata: Some(metadata),
            ..item("out", None)
        }
    }

    #[test]
    fn every_evaluator_variant_compiles_and_scores_identically_per_item_and_in_batch() {
        use EvaluatorSpec::*;
        // A representative instance of every EvaluatorSpec variant — if a new variant is added and
        // not covered here, the match in a maintained list won't catch it, but the intent is that
        // NO variant is silently broken across the batch/per-item paths.
        let specs = vec![
            ExactMatch,
            Contains {
                substring: Some("Par".into()),
            },
            Regex {
                pattern: "P.*s".into(),
            },
            JsonValid,
            Levenshtein { threshold: 0.8 },
            NonEmpty,
            ContainsAll {
                substrings: vec!["a".into()],
            },
            ContainsAny {
                substrings: vec!["z".into(), "a".into()],
            },
            LengthBounds {
                min: Some(1),
                max: Some(100),
            },
            NumericTolerance { tolerance: 0.1 },
            EqualsNumeric,
            JsonSchema {
                schema: serde_json::json!({ "type": "object" }),
            },
            Latency { max_ms: 100.0 },
            Cost { max_usd: 1.0 },
        ];
        let items = vec![item("Paris", Some("Paris")), item("{\"a\":1}", Some("1"))];
        for spec in &specs {
            // Every variant compiles (params validate) — no variant silently broken across paths.
            let compiled = spec
                .compile()
                .unwrap_or_else(|e| panic!("{} must compile: {e}", spec.name()));
            for it in &items {
                // The public per-item API produces exactly what the batch runner's compiled
                // evaluator does — one interface, no per-item vs batch divergence.
                let per_item = spec.evaluate(it).unwrap();
                let batch = compiled.score(it);
                assert_eq!(
                    per_item,
                    batch,
                    "evaluator {} diverges per-item vs batch",
                    spec.name()
                );
            }
        }
    }

    #[test]
    fn levenshtein_similarity_bounds() {
        assert_eq!(levenshtein_similarity("abc", "abc"), 1.0);
        assert_eq!(levenshtein_similarity("", ""), 1.0);
        assert_eq!(levenshtein_similarity("abc", "xyz"), 0.0);
        assert!((levenshtein_similarity("kitten", "sitting") - (1.0 - 3.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn evaluators_score_as_expected() {
        assert_eq!(
            ExactMatch
                .score(&item("Paris", Some("Paris")))
                .unwrap()
                .value,
            1.0
        );
        assert_eq!(
            ExactMatch
                .score(&item(" Paris ", Some("Paris")))
                .unwrap()
                .value,
            1.0
        ); // trimmed
        assert_eq!(
            ExactMatch.score(&item("4", Some("four"))).unwrap().value,
            0.0
        );
        assert!(ExactMatch.score(&item("x", None)).is_none()); // no expected -> skipped

        let c = Contains {
            substring: Some("Par".into()),
        };
        assert_eq!(c.score(&item("Paris", None)).unwrap().value, 1.0);
        assert_eq!(c.score(&item("Tokyo", None)).unwrap().value, 0.0);
        // falls back to expected_output as the needle
        let c2 = Contains { substring: None };
        assert_eq!(
            c2.score(&item("the Eiffel Tower", Some("Eiffel")))
                .unwrap()
                .value,
            1.0
        );

        let r = RegexEval {
            re: regex::Regex::new(r"^\d+$").unwrap(),
        };
        assert_eq!(r.score(&item("42", None)).unwrap().value, 1.0);
        assert_eq!(r.score(&item("4a", None)).unwrap().value, 0.0);

        assert_eq!(
            JsonValid
                .score(&item(r#"{"ok":true}"#, None))
                .unwrap()
                .value,
            1.0
        );
        assert_eq!(JsonValid.score(&item("not json", None)).unwrap().value, 0.0);

        let l = Levenshtein { threshold: 0.8 };
        let s = l.score(&item("colour", Some("color"))).unwrap();
        assert!(s.value > 0.8 && s.passed);
    }

    #[test]
    fn deterministic_set_evaluators_score_as_expected() {
        // non_empty
        assert!(NonEmpty.score(&item("hi", None)).unwrap().passed);
        assert!(!NonEmpty.score(&item("   ", None)).unwrap().passed);

        // contains_all — partial credit in the value, pass only when all present
        let all = ContainsAll {
            substrings: vec!["foo".into(), "bar".into()],
        };
        let s = all.score(&item("foo and bar", None)).unwrap();
        assert_eq!(s.value, 1.0);
        assert!(s.passed);
        let s = all.score(&item("only foo", None)).unwrap();
        assert_eq!(s.value, 0.5);
        assert!(!s.passed);
        assert!(s.comment.as_deref().unwrap().contains("bar"));

        // contains_any
        let any = ContainsAny {
            substrings: vec!["x".into(), "bar".into()],
        };
        assert!(any.score(&item("has bar", None)).unwrap().passed);
        assert!(!any.score(&item("nope", None)).unwrap().passed);

        // length_bounds
        let lb = LengthBounds {
            min: Some(2),
            max: Some(5),
        };
        assert!(lb.score(&item("abc", None)).unwrap().passed);
        assert!(!lb.score(&item("a", None)).unwrap().passed);
        assert!(!lb.score(&item("abcdef", None)).unwrap().passed);

        // numeric_tolerance — within tolerance passes; non-numeric is skipped, not failed
        let nt = NumericTolerance { tolerance: 0.05 };
        assert!(nt.score(&item("3.14", Some("3.16"))).unwrap().passed);
        assert!(!nt.score(&item("3.0", Some("3.2"))).unwrap().passed);
        assert!(nt.score(&item("not a number", Some("3.0"))).is_none());
        assert!(nt.score(&item("3.0", None)).is_none());
    }

    #[test]
    fn tier1_completion_evaluators_score_as_expected() {
        // equals_numeric — representation-tolerant exact equality; non-numeric/absent skipped
        let eq = EqualsNumeric;
        assert!(eq.score(&item("1.0", Some("1"))).unwrap().passed);
        assert!(eq.score(&item("42", Some("42.00"))).unwrap().passed);
        assert!(!eq.score(&item("1.0", Some("1.0001"))).unwrap().passed);
        assert!(eq.score(&item("x", Some("1"))).is_none());
        assert!(eq.score(&item("1", None)).is_none());
        // NaN parses as a number; two NaN outputs should MATCH (IEEE-754 `NaN == NaN` is
        // false, so this needs the explicit is_nan branch), while NaN vs a real number fails.
        assert!(eq.score(&item("NaN", Some("nan"))).unwrap().passed);
        assert!(!eq.score(&item("NaN", Some("1"))).unwrap().passed);

        // json_schema — valid instance passes; constraint violation and non-JSON both FAIL
        // (non-JSON is a fail, not a skip: a schema gate must not wave garbage through)
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer", "minimum": 0}
            }
        });
        let js = EvaluatorSpec::JsonSchema { schema }.compile().unwrap();
        assert!(
            js.score(&item(r#"{"name":"a","age":3}"#, None))
                .unwrap()
                .passed
        );
        let bad = js.score(&item(r#"{"name":"a","age":-1}"#, None)).unwrap();
        assert!(!bad.passed && bad.comment.is_some());
        assert!(!js.score(&item(r#"{"name":"a"}"#, None)).unwrap().passed); // missing required
        let notjson = js.score(&item("nope", None)).unwrap();
        assert!(!notjson.passed && notjson.comment.as_deref().unwrap().contains("JSON"));

        // latency — reads latency_ms (or duration_ns fallback) from metadata; absent -> skipped
        let lat = Latency { max_ms: 100.0 };
        assert!(
            lat.score(&meta_item(serde_json::json!({"latency_ms": 80})))
                .unwrap()
                .passed
        );
        assert!(
            !lat.score(&meta_item(serde_json::json!({"latency_ms": 120})))
                .unwrap()
                .passed
        );
        // duration_ns fallback: 50_000_000 ns = 50 ms
        assert!(
            lat.score(&meta_item(
                serde_json::json!({"duration_ns": 50_000_000u64})
            ))
            .unwrap()
            .passed
        );
        assert!(lat.score(&item("x", None)).is_none()); // no metadata -> skipped

        // cost — reads cost_usd (stringified numbers accepted); absent -> skipped
        let cost = Cost { max_usd: 0.01 };
        assert!(
            cost.score(&meta_item(serde_json::json!({"cost_usd": 0.005})))
                .unwrap()
                .passed
        );
        assert!(
            !cost
                .score(&meta_item(serde_json::json!({"cost_usd": "0.02"})))
                .unwrap()
                .passed
        );
        assert!(cost
            .score(&meta_item(serde_json::json!({"other": 1})))
            .is_none());
    }

    #[test]
    fn build_rejects_empty_and_inverted_params() {
        // contains_all/any need a non-empty list; length_bounds needs at least one bound and
        // min<=max; numeric_tolerance must be finite and non-negative.
        assert!(EvaluatorSpec::ContainsAll { substrings: vec![] }
            .compile()
            .is_err());
        assert!(EvaluatorSpec::ContainsAny { substrings: vec![] }
            .compile()
            .is_err());
        assert!(EvaluatorSpec::LengthBounds {
            min: None,
            max: None
        }
        .compile()
        .is_err());
        assert!(EvaluatorSpec::LengthBounds {
            min: Some(10),
            max: Some(2)
        }
        .compile()
        .is_err());
        assert!(EvaluatorSpec::NumericTolerance { tolerance: -1.0 }
            .compile()
            .is_err());
        // latency/cost bounds must be finite and non-negative; a malformed schema is rejected
        // up front (before any item is scored).
        assert!(EvaluatorSpec::Latency { max_ms: -1.0 }.compile().is_err());
        assert!(EvaluatorSpec::Cost { max_usd: f64::NAN }.compile().is_err());
        assert!(EvaluatorSpec::JsonSchema {
            schema: serde_json::json!("not a schema object")
        }
        .compile()
        .is_err());
        assert!(EvaluatorSpec::EqualsNumeric.compile().is_ok());
        assert!(EvaluatorSpec::JsonSchema {
            schema: serde_json::json!({"type": "object"})
        }
        .compile()
        .is_ok());
        assert!(EvaluatorSpec::NonEmpty.compile().is_ok());
    }

    #[test]
    fn load_dataset_parses_jsonl_and_skips_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        std::fs::write(
            &path,
            "{\"output\":\"a\",\"expected_output\":\"a\"}\n\n{\"output\":\"b\",\"span_id\":\"ab\"}\n",
        )
        .unwrap();
        let items = load_dataset(&path).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].span_id.as_deref(), Some("ab"));
    }

    #[test]
    fn run_eval_aggregates_and_gates_on_threshold() {
        let config = EvalConfig {
            name: Some("t".into()),
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::ExactMatch],
            judges: vec![],
            thresholds: BTreeMap::from([("exact_match".to_string(), 0.9)]),
        };
        // 3/4 exact-match -> mean 0.75 < 0.9 -> fails the gate.
        let dataset = vec![
            item("a", Some("a")),
            item("b", Some("b")),
            item("c", Some("c")),
            item("X", Some("c")),
        ];
        let (report, scores) = run_eval(&config, &dataset, "run1", 1).unwrap();
        let agg = &report.aggregates[0];
        assert_eq!(agg.scored, 4);
        assert!((agg.mean - 0.75).abs() < 1e-9);
        assert!(!agg.threshold_met);
        assert!(!report.passed());
        // No spans on the items, so only the one aggregate score is persisted.
        assert_eq!(scores.len(), 1);
        assert!(matches!(scores[0].target, ScoreTarget::Run(_)));
        assert_eq!(scores[0].num_value, Some(0.75));
    }

    #[test]
    fn run_eval_attaches_per_item_scores_to_spans() {
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::ExactMatch],
            judges: vec![],
            thresholds: BTreeMap::new(),
        };
        let mut a = item("a", Some("a"));
        a.span_id = Some("span-1".into());
        let (_report, scores) = run_eval(&config, &[a], "run2", 1).unwrap();
        // one per-item score (targeting the span) + one aggregate (targeting the run)
        assert_eq!(scores.len(), 2);
        assert!(scores
            .iter()
            .any(|s| s.target == ScoreTarget::Span("span-1".into())));
        assert!(scores
            .iter()
            .any(|s| matches!(s.target, ScoreTarget::Run(_))));
    }

    #[test]
    fn no_threshold_means_no_gate() {
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::JsonValid],
            judges: vec![],
            thresholds: BTreeMap::new(),
        };
        let (report, _) = run_eval(&config, &[item("not json", None)], "r", 1).unwrap();
        assert!(report.passed()); // 0.0 mean but no threshold configured
    }

    #[test]
    fn run_eval_rejects_duplicate_evaluator_names() {
        // Two `contains` evaluators share the name "contains" -> colliding Score ids and a
        // name-keyed aggregate map. Reject before any silent clobbering happens.
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![
                EvaluatorSpec::Contains {
                    substring: Some("a".into()),
                },
                EvaluatorSpec::Contains {
                    substring: Some("b".into()),
                },
            ],
            judges: vec![],
            thresholds: BTreeMap::new(),
        };
        let err = run_eval(&config, &[item("ab", None)], "r", 1).unwrap_err();
        assert!(err.to_string().contains("duplicate evaluator"), "{err}");
    }

    #[test]
    fn all_skipped_evaluator_does_not_fail_gate() {
        // exact_match needs `expected_output`; no item has one, so it scores nothing.
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::ExactMatch],
            judges: vec![],
            thresholds: BTreeMap::from([("exact_match".to_string(), 0.9)]),
        };
        let (report, scores) =
            run_eval(&config, &[item("x", None), item("y", None)], "r", 1).unwrap();
        let agg = &report.aggregates[0];
        assert_eq!(agg.scored, 0);
        assert!(
            agg.threshold_met,
            "an all-skipped evaluator must not fail the gate on a fabricated 0.0 mean"
        );
        assert!(report.passed());
        // The aggregate Score is still persisted (mean 0.0 over 0 items) for the record.
        assert_eq!(scores.len(), 1);
    }

    #[tokio::test]
    async fn run_command_persists_scores_and_returns_gate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("eval.yaml");
        std::fs::write(
            dir.path().join("data.jsonl"),
            "{\"output\":\"Paris\",\"expected_output\":\"Paris\"}\n\
             {\"output\":\"wrong\",\"expected_output\":\"Tokyo\"}\n",
        )
        .unwrap();
        std::fs::write(
            &cfg_path,
            "name: t\ndataset: data.jsonl\nevaluators:\n  - type: exact_match\nthresholds:\n  exact_match: 0.9\n",
        )
        .unwrap();

        let data_dir = dir.path().join("store");
        let passed = run_command(&cfg_path, &data_dir).await.unwrap();
        assert!(!passed, "0.5 exact-match < 0.9 threshold -> gate fails");

        // The aggregate score was persisted and is retrievable for `eval compare`.
        let store = Store::open(
            &data_dir,
            StoreConfig {
                compact_interval: None,
                ..Default::default()
            },
        )
        .unwrap();
        let run_scores = store.list_scores(100).unwrap();
        assert!(run_scores
            .iter()
            .any(|s| s.name == "exact_match" && s.num_value == Some(0.5)));
    }

    fn test_store(dir: &std::path::Path) -> Store {
        Store::open(
            dir,
            StoreConfig {
                compact_interval: None,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn persist_run(
        store: &Store,
        evaluators: Vec<EvaluatorSpec>,
        dataset: &[DatasetItem],
        run_id: &str,
    ) {
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators,
            judges: vec![],
            thresholds: BTreeMap::new(),
        };
        let (_report, scores) = run_eval(&config, dataset, run_id, 1).unwrap();
        store.put_scores(&scores).unwrap();
    }

    #[tokio::test]
    async fn compare_runs_diffs_aggregate_means() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // run A: 1/2 exact-match = 0.5; run B: 2/2 = 1.0 (an improvement)
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("a", Some("a")), item("X", Some("b"))],
            "runA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("a", Some("a")), item("b", Some("b"))],
            "runB",
        );

        let report = compare_runs(&store, "runA", "runB", 0.05).unwrap();
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.evaluator, "exact_match");
        assert_eq!(row.a, Some(0.5));
        assert_eq!(row.b, Some(1.0));
        assert!((row.delta.unwrap() - 0.5).abs() < 1e-9);
        assert!(!report.has_regression(0.0)); // an improvement is not a regression
    }

    #[tokio::test]
    async fn compare_runs_flags_regression_with_tolerance() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // A: 2/2 = 1.0; B: 1/2 = 0.5 -> delta -0.5
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("a", Some("a")), item("b", Some("b"))],
            "rA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("a", Some("a")), item("X", Some("b"))],
            "rB",
        );

        let report = compare_runs(&store, "rA", "rB", 0.05).unwrap();
        assert!((report.rows[0].delta.unwrap() - (-0.5)).abs() < 1e-9);
        assert!(report.has_regression(0.0));
        assert!(report.has_regression(0.4));
        assert!(!report.has_regression(0.5)); // exactly tolerance -> not *beyond* it
        assert!(!report.has_regression(0.6));
    }

    #[tokio::test]
    async fn compare_runs_unions_differing_evaluator_sets() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // run A has two evaluators; run B only one. The diff unions both names.
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch, EvaluatorSpec::JsonValid],
            &[item("{}", Some("{}"))],
            "uA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("{}", Some("{}"))],
            "uB",
        );

        let report = compare_runs(&store, "uA", "uB", 0.05).unwrap();
        assert_eq!(report.rows.len(), 2);
        let jv = report
            .rows
            .iter()
            .find(|r| r.evaluator == "json_valid")
            .unwrap();
        assert_eq!(jv.a, Some(1.0));
        assert_eq!(jv.b, None);
        assert_eq!(jv.delta, None);
        // An evaluator missing on one side is surfaced but never counts as a regression.
        assert!(!report.has_regression(0.0));
    }

    #[tokio::test]
    async fn compare_runs_errors_on_unknown_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch],
            &[item("a", Some("a"))],
            "known",
        );

        let err = compare_runs(&store, "known", "ghost", 0.05).unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
        let err2 = compare_runs(&store, "ghost", "known", 0.05).unwrap_err();
        assert!(err2.to_string().contains("ghost"), "{err2}");
    }

    #[tokio::test]
    async fn compare_command_gates_on_regression() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            persist_run(
                &store,
                vec![EvaluatorSpec::ExactMatch],
                &[item("a", Some("a")), item("b", Some("b"))],
                "good",
            );
            persist_run(
                &store,
                vec![EvaluatorSpec::ExactMatch],
                &[item("a", Some("a")), item("X", Some("b"))],
                "bad",
            );
        } // drop the store so compare_command can reopen the redb

        // good(1.0) -> bad(0.5): a regression. With the gate on, that's exit-nonzero.
        let passed = compare_command("good", "bad", &data_dir, true, 0.0, false, 0.05)
            .await
            .unwrap();
        assert!(!passed);
        // Without the gate, compare is informational and always "passes".
        let passed = compare_command("good", "bad", &data_dir, false, 0.0, false, 0.05)
            .await
            .unwrap();
        assert!(passed);
        // The reverse direction is an improvement -> passes even with the gate on.
        let passed = compare_command("bad", "good", &data_dir, true, 0.0, false, 0.05)
            .await
            .unwrap();
        assert!(passed);
    }

    // The aggregate Score now carries sufficient statistics so `eval compare` can run a
    // significance test. These cover: the verdict is attached, a real difference on enough
    // items is flagged significant, a small-sample drop is forgiven as noise in significance
    // mode, and a stats-less (legacy) run falls back to the raw-delta gate.

    /// Build a run whose aggregate carries explicit stats, by persisting `n` ExactMatch items
    /// of which `passes` pass. Returns the run id.
    fn persist_binary_run(store: &Store, run_id: &str, n: usize, passes: usize) {
        let mut items = Vec::with_capacity(n);
        for i in 0..n {
            // "a"/"a" passes; "x"/"a" fails — exact_match value is 1.0/0.0.
            let out = if i < passes { "a" } else { "x" };
            items.push(item(out, Some("a")));
        }
        persist_run(store, vec![EvaluatorSpec::ExactMatch], &items, run_id);
    }

    #[tokio::test]
    async fn compare_attaches_significance_and_flags_real_regression() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // 90/100 -> 75/100: a real 0.15 drop on a large sample — decisively significant.
        persist_binary_run(&store, "bigA", 100, 90);
        persist_binary_run(&store, "bigB", 100, 75);

        let report = compare_runs(&store, "bigA", "bigB", 0.05).unwrap();
        let row = &report.rows[0];
        assert_eq!(row.n_a, Some(100));
        assert_eq!(row.n_b, Some(100));
        let s = row.signif.expect("both sides have stats -> a test");
        assert!(s.p_two_sided < 0.05, "p={}", s.p_two_sided);
        assert!(
            s.ci_high < 0.0,
            "95% CI excludes 0: {:?}",
            (s.ci_low, s.ci_high)
        );
        // It IS a regression and IS significant -> the significance gate fires.
        assert!(report.has_regression(0.0));
        assert!(report.has_gating_significant_regression(0.0));
    }

    #[tokio::test]
    async fn compare_forgives_small_sample_noise_in_significance_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // 9/10 -> 8/10: a 0.10 drop, but on n=10 it is within sampling noise.
        persist_binary_run(&store, "smallA", 10, 9);
        persist_binary_run(&store, "smallB", 10, 8);

        let report = compare_runs(&store, "smallA", "smallB", 0.05).unwrap();
        // Raw-delta gate still sees a regression...
        assert!(report.has_regression(0.0));
        // ...but the significance gate forgives it as noise (CI straddles 0).
        assert!(!report.has_gating_significant_regression(0.0));
        let s = report.rows[0].signif.unwrap();
        assert!(
            s.ci_low < 0.0 && s.ci_high > 0.0,
            "CI straddles 0: {:?}",
            (s.ci_low, s.ci_high)
        );
    }

    #[tokio::test]
    async fn significance_gate_falls_back_when_a_run_lacks_stats() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // Run A is a "legacy" aggregate: a Run-targeted Score with a mean but no agg_stats.
        let legacy = Score {
            id: "legacy:agg:exact_match".to_string(),
            target: ScoreTarget::Run("legacy".to_string()),
            name: "exact_match".to_string(),
            num_value: Some(1.0),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: None, // <- predates the stats field
            ts_unix_nano: 1,
        };
        store.put_scores(&[legacy]).unwrap();
        persist_binary_run(&store, "newrun", 10, 5); // 0.5 mean, with stats

        let report = compare_runs(&store, "legacy", "newrun", 0.05).unwrap();
        let row = &report.rows[0];
        assert!((row.delta.unwrap() - (-0.5)).abs() < 1e-9);
        assert!(row.signif.is_none(), "no test when one side lacks stats");
        // Untestable regression: forgiven by neither gate — it still gates in significance mode
        // (we don't let a real drop slip through just because a run predates the stats field).
        assert!(report.has_regression(0.0));
        assert!(report.has_gating_significant_regression(0.0));
    }

    // These exercise the COMMAND boundary (`compare_command` + its gate/exit wiring) in
    // significance mode — the exact CLI path the feature exists to protect, which the
    // compare_runs-level tests above bypass.

    #[tokio::test]
    async fn compare_command_significance_gate_fires_and_forgives() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            persist_binary_run(&store, "bigA", 100, 90); // 0.90
            persist_binary_run(&store, "bigB", 100, 75); // 0.75 — a decisive, significant drop
            persist_binary_run(&store, "smA", 10, 9); // 0.90
            persist_binary_run(&store, "smB", 10, 8); // 0.80 — a small-sample noise drop
        } // drop the store so compare_command can reopen the redb

        // Significant regression -> the significance gate fails the build (exit non-zero).
        let passed = compare_command("bigA", "bigB", &data_dir, true, 0.0, true, 0.05)
            .await
            .unwrap();
        assert!(!passed, "a significant 0.15 drop on n=100 must gate");

        // The SAME small drop: forgiven as noise in significance mode, but flagged by the
        // default raw-delta gate — proving --significance is what changes the verdict.
        let passed_sig = compare_command("smA", "smB", &data_dir, true, 0.0, true, 0.05)
            .await
            .unwrap();
        assert!(
            passed_sig,
            "a 0.10 drop on n=10 is within noise -> significance gate passes"
        );
        let passed_raw = compare_command("smA", "smB", &data_dir, true, 0.0, false, 0.05)
            .await
            .unwrap();
        assert!(
            !passed_raw,
            "the raw-delta gate still fails on the same drop"
        );
    }

    #[tokio::test]
    async fn compare_command_significance_still_gates_untestable() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            // A legacy aggregate (mean only, no stats) on the baseline side.
            let legacy = Score {
                id: "legacy:agg:exact_match".to_string(),
                target: ScoreTarget::Run("legacy".to_string()),
                name: "exact_match".to_string(),
                num_value: Some(1.0),
                str_value: None,
                data_type: DataType::Numeric,
                source: ScoreSource::Eval,
                comment: None,
                config_id: None,
                agg_stats: None,
                ts_unix_nano: 1,
            };
            store.put_scores(&[legacy]).unwrap();
            persist_binary_run(&store, "newrun", 50, 25); // 0.50, with stats
        }

        // An untestable regression (baseline predates the stats field) must STILL gate in
        // significance mode — a real drop never slips through silently.
        let passed = compare_command("legacy", "newrun", &data_dir, true, 0.0, true, 0.05)
            .await
            .unwrap();
        assert!(
            !passed,
            "a real drop must gate even when one run lacks stats"
        );
    }

    #[tokio::test]
    async fn variance_is_correct_for_a_graded_evaluator() {
        // The significance test claims to work for GRADED scorers (value in [0,1]), not just
        // binary 0/1. Verify run_eval's variance accumulation over non-{0,1} values end-to-end.
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        // contains_all over ["a","b"]: fraction-present values for these outputs are
        // [0.5 ("a"), 1.0 ("ab"), 0.5 ("b"), 0.0 ("x")] -> mean 0.5, ddof=1 variance 0.5/3.
        let items = [
            item("a", None),
            item("ab", None),
            item("b", None),
            item("x", None),
        ];
        persist_run(
            &store,
            vec![EvaluatorSpec::ContainsAll {
                substrings: vec!["a".into(), "b".into()],
            }],
            &items,
            "graded",
        );

        let aggs = store
            .scores_for_target(&ScoreTarget::Run("graded".to_string()))
            .unwrap();
        let agg = aggs
            .iter()
            .find(|s| s.id == "graded:agg:contains_all")
            .expect("aggregate score persisted");
        let st = agg.agg_stats.expect("graded aggregate carries stats");
        assert_eq!(st.n, 4);
        assert_eq!(st.pass_count, 1); // only "ab" passes (all substrings present)
        assert!((st.mean - 0.5).abs() < 1e-12, "mean={}", st.mean);
        assert!(
            (st.variance - 0.5 / 3.0).abs() < 1e-12,
            "ddof=1 variance should be 0.5/3, got {}",
            st.variance
        );
    }
}
