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

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::model::AggStats;
use crate::stats::{bootstrap_mean_ci, paired_t_test, welch_t_test, WelchResult};
use crate::{DataType, Score, ScoreSource, ScoreTarget, Store, StoreConfig};

/// One dataset row (JSONL). `output` is the actual text to score; `expected_output` is
/// the reference for comparison evaluators; `span_id`/`trace_id` (optional) say where to
/// attach the per-item score so it surfaces on the trace.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatasetItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Retrieved context passages (RAG) — the grounding for `faithfulness` / `hallucination`
    /// judge rails. Absent for non-RAG datasets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    /// What identifies this item across runs, so two runs' scores on it can be paired: its
    /// `id`, else its span, else its trace. `None` for an item with none of them.
    pub fn key(&self) -> Option<&str> {
        [&self.id, &self.span_id, &self.trace_id]
            .into_iter()
            .find_map(|k| k.as_deref().filter(|k| !k.is_empty()))
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

/// The eval run config (YAML): which dataset, which evaluators, and the pass thresholds. An
/// unknown top-level key is an error rather than silently ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalConfig {
    #[serde(default)]
    pub name: Option<String>,
    /// Path to the JSONL dataset (relative paths resolve against the config file's dir).
    pub dataset: PathBuf,
    /// Tier-1 deterministic evaluators (default empty so a judges-only config is valid).
    #[serde(default)]
    pub evaluators: Vec<EvaluatorSpec>,
    /// Where judges' model calls go: hosted providers or the team's own OpenAI-compatible
    /// servers, each declared once by name ([`crate::endpoint`]).
    #[serde(default)]
    pub endpoints: Vec<crate::endpoint::EndpointSpec>,
    /// Tier-3 LLM-as-judge evaluators (BYO-key; requires building with `--features judge`).
    #[serde(default)]
    pub judges: Vec<crate::judge::JudgeSpec>,
    /// Per-evaluator minimum mean score for the run to pass (CI gate).
    #[serde(default)]
    pub thresholds: BTreeMap<String, f64>,
    /// The item `metadata` keys a run records for slicing (`eval compare --non-inferior` gives
    /// a verdict per value). Unset: `language`, `route` and `task`.
    #[serde(default)]
    pub slice_by: Option<Vec<String>>,
}

/// The `metadata` keys a run slices by when the config names none.
pub const DEFAULT_SLICE_KEYS: [&str; 3] = ["language", "route", "task"];

impl EvalConfig {
    /// The `metadata` keys this run records for slicing.
    pub fn slice_keys(&self) -> Vec<String> {
        match &self.slice_by {
            Some(keys) => keys.clone(),
            None => DEFAULT_SLICE_KEYS.map(String::from).to_vec(),
        }
    }
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
    /// `output == expected_output` (trimmed). `normalize: true` compares them Unicode-normalised
    /// (NFKC, lower-cased; [`crate::lang::normalize`]).
    ExactMatch {
        #[serde(default)]
        normalize: bool,
    },
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
        /// Compare Unicode-normalised, as `exact_match`'s `normalize`.
        #[serde(default)]
        normalize: bool,
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
    /// `output` is in the expected language: `language` (an ISO 639 code), else the item's
    /// `metadata.language`. Detected by an embedded detector ([`crate::lang`]); Malay and
    /// Indonesian count as one. Skipped when no language is expected, or the detector is unsure
    /// (too little text).
    LanguageMatch {
        #[serde(default)]
        language: Option<String>,
    },
    /// The share of `output`'s letters in the expected script: `script` (`Latin`, `Thai`,
    /// `Tamil`, `Han`, `Jawi`, …), else the script(s) of the item's `metadata.language`. Passes
    /// at `>= min`. Skipped when neither says, or the output has no letters.
    Script {
        #[serde(default)]
        script: Option<String>,
        #[serde(default = "default_script_min")]
        min: f64,
    },
}

fn default_script_min() -> f64 {
    0.8
}

fn default_levenshtein_threshold() -> f64 {
    0.8
}

impl EvaluatorSpec {
    /// The evaluator's stable name (also its threshold key).
    pub fn name(&self) -> &'static str {
        match self {
            EvaluatorSpec::ExactMatch { .. } => "exact_match",
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
            EvaluatorSpec::LanguageMatch { .. } => "language_match",
            EvaluatorSpec::Script { .. } => "script",
        }
    }

    /// Compile to a runnable [`Evaluator`], validating params (e.g. the regex). This is the **single
    /// evaluator interface**: the batch runner ([`run_eval`]), the per-item [`Self::evaluate`], and
    /// any experiment path all funnel through the same compiled [`Evaluator::score`], so there is no
    /// batch-only vs per-item scoring divergence (a split batch/per-item interface is a known source
    /// of scoring drift; a single interface avoids it by construction).
    pub fn compile(&self) -> anyhow::Result<Box<dyn Evaluator>> {
        Ok(match self {
            EvaluatorSpec::ExactMatch { normalize } => Box::new(ExactMatch {
                normalize: *normalize,
            }),
            EvaluatorSpec::Contains { substring } => Box::new(Contains {
                substring: substring.clone(),
            }),
            EvaluatorSpec::Regex { pattern } => Box::new(RegexEval {
                re: regex::Regex::new(pattern)
                    .map_err(|e| anyhow::anyhow!("invalid regex {pattern:?}: {e}"))?,
            }),
            EvaluatorSpec::JsonValid => Box::new(JsonValid),
            EvaluatorSpec::Levenshtein {
                threshold,
                normalize,
            } => Box::new(Levenshtein {
                threshold: *threshold,
                normalize: *normalize,
            }),
            EvaluatorSpec::LanguageMatch { language } => Box::new(LanguageMatch {
                language: language.clone(),
            }),
            EvaluatorSpec::Script { script, min } => {
                anyhow::ensure!(
                    min.is_finite() && (0.0..=1.0).contains(min),
                    "script: `min` is a share of the letters, between 0 and 1"
                );
                let expected = match script {
                    Some(names) => Some(
                        names
                            .split(',')
                            .map(|n| {
                                crate::lang::Script::parse(n).ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "script: unknown script {n:?} (Latin, Thai, Tamil, Han, \
                                         Arabic or Jawi, Devanagari, …)"
                                    )
                                })
                            })
                            .collect::<anyhow::Result<Vec<_>>>()?,
                    ),
                    None => None,
                };
                Box::new(ScriptEval {
                    expected,
                    min: *min,
                })
            }
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

struct ExactMatch {
    normalize: bool,
}
impl Evaluator for ExactMatch {
    fn name(&self) -> &str {
        "exact_match"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected = item.expected_output.as_deref()?;
        let pass = if self.normalize {
            crate::lang::normalize(&item.output) == crate::lang::normalize(expected)
        } else {
            item.output.trim() == expected.trim()
        };
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
    normalize: bool,
}
impl Evaluator for Levenshtein {
    fn name(&self) -> &str {
        "levenshtein"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected = item.expected_output.as_deref()?;
        let value = if self.normalize {
            levenshtein_similarity(
                &crate::lang::normalize(&item.output),
                &crate::lang::normalize(expected),
            )
        } else {
            levenshtein_similarity(&item.output, expected)
        };
        Some(ItemScore {
            passed: value >= self.threshold,
            value,
            comment: None,
        })
    }
}

/// The item's expected language: the evaluator's own, else `metadata.language`.
fn expected_language<'a>(own: Option<&'a str>, item: &'a DatasetItem) -> Option<&'a str> {
    own.or_else(|| {
        item.metadata
            .as_ref()?
            .get("language")?
            .as_str()
            .filter(|s| !s.is_empty())
    })
}

struct LanguageMatch {
    language: Option<String>,
}
impl Evaluator for LanguageMatch {
    fn name(&self) -> &str {
        "language_match"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected = expected_language(self.language.as_deref(), item)?;
        // Unsure is not wrong: too little text to tell is skipped, not failed.
        let found = crate::lang::detect(&item.output).filter(|d| d.reliable)?;
        let pass = crate::lang::same_language(expected, &found);
        Some(ItemScore {
            value: pass as u8 as f64,
            passed: pass,
            comment: Some(format!(
                "expected {expected}, detected {} ({}, confidence {:.2})",
                found.code, found.name, found.confidence
            )),
        })
    }
}

struct ScriptEval {
    expected: Option<Vec<crate::lang::Script>>,
    min: f64,
}
impl Evaluator for ScriptEval {
    fn name(&self) -> &str {
        "script"
    }
    fn score(&self, item: &DatasetItem) -> Option<ItemScore> {
        let expected: &[crate::lang::Script] = match &self.expected {
            Some(s) => s,
            None => crate::lang::scripts_for(expected_language(None, item)?)?,
        };
        let share = crate::lang::script_share(&item.output, expected)?;
        Some(ItemScore {
            value: share,
            passed: share >= self.min,
            comment: Some(format!(
                "{:.0}% of the letters in {}",
                share * 100.0,
                expected
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join("/")
            )),
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
    /// Each evaluator's mean by slice ([`slice_means`]); empty until the run is stored.
    pub slices: Vec<SliceMean>,
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
            slices: Vec::new(),
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
    let mut per_item = ItemValues {
        evaluator: name.to_string(),
        ..ItemValues::default()
    };

    for (idx, (item, outcome)) in dataset.iter().zip(outcomes).enumerate() {
        let Some(outcome) = outcome else {
            continue; // skipped — missing required field, judge parse failure, etc.
        };
        values.push(outcome.value);
        match item.key() {
            Some(k) => per_item.items.push((k.to_string(), outcome.value)),
            None => per_item.unkeyed += 1,
        }
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

    // Every scored item's value under its key, for a paired comparison and a repeat-stability
    // check: one run-targeted Score per evaluator, whatever the items attach to.
    scores.push(per_item.score(run_id, ts_unix_nano));

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
    run_command_junit(config_path, data_dir, None, None, false).await
}

/// [`run_command`] that also writes a JUnit XML report to `junit` (see [`crate::junit`]). The
/// report is written whether the gate passes, fails, or the run errors; the returned gate
/// verdict is exactly what [`run_command`] would have returned.
/// `fresh` judges every item again instead of serving cached judge scores (`--no-cache`).
pub async fn run_command_junit(
    config_path: &Path,
    data_dir: &Path,
    junit: Option<&Path>,
    report_json: Option<&Path>,
    fresh: bool,
) -> anyhow::Result<bool> {
    let started = std::time::Instant::now();
    let result = run_report(config_path, data_dir, fresh).await;
    crate::junit::write_after(
        junit,
        &result,
        crate::junit::from_run,
        "evald eval run",
        "evald.eval.run",
        started.elapsed().as_secs_f64(),
    )?;
    crate::report::write_after(report_json, &result, crate::report::run, "eval_run")?;
    result.map(|report| report.passed())
}

/// The body of `eval run`: evaluate, persist, print, and hand back the report.
async fn run_report(config_path: &Path, data_dir: &Path, fresh: bool) -> anyhow::Result<RunReport> {
    let config = load_config(config_path)?;
    let dataset_path = resolve_relative(config_path, &config.dataset);
    let dataset = load_dataset(&dataset_path)?;
    anyhow::ensure!(
        !dataset.is_empty(),
        "dataset {} has no items",
        dataset_path.display()
    );
    check_names(&config)?;
    let run = RunInputs {
        config: &config,
        config_path,
        dataset_path: &dataset_path,
        dataset: &dataset,
        run_id: uuid::Uuid::new_v4().to_string(),
    };
    evaluate_and_store(run, data_dir, fresh, Extra::default()).await
}

/// Every Tier-1 evaluator name + judge name shares one space (scores + thresholds key by
/// name). Rejected UP FRONT — before any judge (or replay) makes a paid LLM call.
fn check_names(config: &EvalConfig) -> anyhow::Result<()> {
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
    Ok(())
}

/// What one run evaluates: the config, its dataset (as loaded, or as replayed), and its id.
struct RunInputs<'a> {
    config: &'a EvalConfig,
    config_path: &'a Path,
    dataset_path: &'a Path,
    dataset: &'a [DatasetItem],
    run_id: String,
}

/// What a run stores beyond its scores: a replay's spans (and egress) and its summary.
#[derive(Default)]
struct Extra {
    spans: Vec<crate::model::NormalizedSpan>,
    replay: Option<crate::replay::ReplaySummary>,
}

/// Score `run.dataset` with the config's evaluators and judges, persist the scores, the
/// manifest, the slice labels and every span, print the report, and hand it back.
async fn evaluate_and_store(
    run: RunInputs<'_>,
    data_dir: &Path,
    fresh: bool,
    extra: Extra,
) -> anyhow::Result<RunReport> {
    let RunInputs {
        config,
        config_path,
        dataset_path,
        dataset,
        run_id,
    } = run;
    let run_id = run_id.as_str();
    let ts = now_unix_nano();
    let (mut report, mut scores) = run_eval(config, dataset, run_id, ts)?;

    // Tier-3 judges (BYO-key, off by default) append to the SAME report + scores, so they flow
    // through the threshold gate, persistence, and `eval compare` significance like Tier-1.
    let mut judged = crate::judge::JudgeOutput::default();
    if !config.judges.is_empty() {
        let endpoints = crate::endpoint::resolve_all(&config.endpoints, &config_dir(config_path))?;
        judged = match crate::judge::run_all_judges(
            &config.judges,
            &endpoints,
            dataset,
            run_id,
            ts,
            data_dir,
            &config.thresholds,
            &mut scores,
            fresh,
        )
        .await
        {
            Ok(j) => j,
            Err(e) => {
                // The calls made (or refused) before the failure still go in the ledger.
                if let Some(failed) = e.downcast_ref::<crate::judge::FailedRun>() {
                    record_egress(data_dir, &failed.egress, run_id).await;
                }
                return Err(e);
            }
        };
        report.aggregates.append(&mut judged.aggregates);
    }
    // Each judge call as an `evald.judge` span, every outbound request as an `evald.egress`
    // one, and what decided this run's scores.
    let mut judge_spans = crate::judge::call_spans(run_id, &judged.judges);
    judge_spans.extend(crate::egress::spans(&judged.egress, run_id));
    judge_spans.extend(extra.spans);
    scores.push(manifest_score(
        config,
        dataset_path,
        dataset.len(),
        run_id,
        ts,
        &judged.judges,
        extra.replay.as_ref(),
    ));
    // The items' slice labels, for `eval compare --non-inferior` to give a verdict per slice.
    let slices = RunSlices::from_items(&config.slice_keys(), dataset);
    scores.extend(slices.score(run_id, ts));

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
        .ingest_batch(judge_spans, &scores)
        .await
        .map_err(|e| anyhow::anyhow!("persisting scores: {e}"))?;

    report.slices = slice_means(&scores, &slices);
    print_report(&report, dataset_path);
    print_slice_means(&report.slices);
    Ok(report)
}

/// One evaluator's mean over the items of one slice value.
#[derive(Debug, Clone, PartialEq)]
pub struct SliceMean {
    pub key: String,
    pub value: String,
    pub evaluator: String,
    pub n: usize,
    pub mean: f64,
}

/// Each evaluator's mean per slice value, from a run's per-item values and its slice labels,
/// for every slice key with at least two values (one value says nothing a run's mean does not).
pub fn slice_means(scores: &[Score], slices: &RunSlices) -> Vec<SliceMean> {
    let values: Vec<ItemValues> = scores
        .iter()
        .filter(|s| s.name == ITEMS_NAME)
        .filter_map(|s| serde_json::from_str(s.str_value.as_deref()?).ok())
        .collect();
    let mut out = Vec::new();
    for (key, labels) in &slices.by {
        let distinct: BTreeSet<&String> = labels.values().collect();
        if distinct.len() < 2 {
            continue;
        }
        for v in &values {
            let mut by: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
            for (item, x) in &v.items {
                if let Some(label) = labels.get(item) {
                    let e = by.entry(label.as_str()).or_default();
                    e.0 += 1;
                    e.1 += x;
                }
            }
            out.extend(by.into_iter().map(|(value, (n, sum))| SliceMean {
                key: key.clone(),
                value: value.to_string(),
                evaluator: v.evaluator.clone(),
                n,
                mean: sum / n as f64,
            }));
        }
    }
    out
}

/// The run's means by slice, under its report.
fn print_slice_means(rows: &[SliceMean]) {
    let mut key = None;
    for r in rows {
        if key != Some(&r.key) {
            println!(
                "\nby {:<18} {:<22} {:>6} {:>7}",
                r.key, "evaluator", "items", "mean"
            );
            key = Some(&r.key);
        }
        println!(
            "   {:<18} {:<22} {:>6} {:>7.3}",
            r.value, r.evaluator, r.n, r.mean
        );
    }
}

/// Where `evald replay` sends the dataset: an endpoint the config declares, or a server named
/// on the command line.
#[derive(Debug, Clone)]
pub enum ReplayTarget {
    /// A name from the config's `endpoints:` (or `openai` / `anthropic`, the implicit ones).
    Named(String),
    /// A server given by its URL, the API it speaks and the variable holding its key.
    Url {
        base_url: String,
        api: String,
        api_key_env: Option<String>,
    },
}

/// The [`crate::endpoint::Endpoint`] a [`ReplayTarget`] names.
pub fn replay_endpoint(
    config: &EvalConfig,
    config_path: &Path,
    target: &ReplayTarget,
) -> anyhow::Result<crate::endpoint::Endpoint> {
    use crate::endpoint::{Api, Endpoint, EndpointSpec, IMPLICIT_ANTHROPIC, IMPLICIT_OPENAI};
    match target {
        ReplayTarget::Named(name) if name == IMPLICIT_OPENAI => Endpoint::implicit(Api::OpenaiChat),
        ReplayTarget::Named(name) if name == IMPLICIT_ANTHROPIC => {
            Endpoint::implicit(Api::AnthropicMessages)
        }
        ReplayTarget::Named(name) => {
            let mut endpoints =
                crate::endpoint::resolve_all(&config.endpoints, &config_dir(config_path))?;
            endpoints.remove(name).with_context(|| {
                format!(
                    "{} declares no endpoint named {name:?} (it has: {})",
                    config_path.display(),
                    endpoints.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })
        }
        ReplayTarget::Url {
            base_url,
            api,
            api_key_env,
        } => {
            let spec: EndpointSpec = serde_json::from_value(serde_json::json!({
                "name": "replay",
                "api": api,
                "base_url": base_url,
                "api_key_env": api_key_env,
            }))?;
            spec.resolve(Path::new("."))
        }
    }
}

/// `evald replay` — send the config's dataset prompts to `model` on `target`, and score the
/// answers with the config's evaluators and judges as a run of their own: the candidate run of
/// a migration gate. Returns whether its thresholds passed, as `eval run` does. `out` also
/// writes the candidate dataset (the items with the replay's answers) as JSONL.
pub async fn replay_command(
    config_path: &Path,
    target: &ReplayTarget,
    model: &str,
    data_dir: &Path,
    fresh: bool,
    out: Option<&Path>,
) -> anyhow::Result<bool> {
    let config = load_config(config_path)?;
    let dataset_path = resolve_relative(config_path, &config.dataset);
    let dataset = load_dataset(&dataset_path)?;
    check_names(&config)?;
    crate::replay::check_items(&dataset)
        .with_context(|| format!("dataset {}", dataset_path.display()))?;
    let ep = replay_endpoint(&config, config_path, target)?;

    #[cfg(not(feature = "judge"))]
    {
        let _ = (model, data_dir, fresh, out, ep);
        anyhow::bail!(
            "this build has no HTTP client — rebuild with `--features judge` to replay a dataset \
             against an endpoint"
        );
    }

    #[cfg(feature = "judge")]
    {
        use crate::judge::JudgeBackend;
        let run_id = uuid::Uuid::new_v4().to_string();
        let backend = crate::judge::replay_backend();
        let provenance = match backend.provenance(&ep, model).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(endpoint = %ep.name, error = %format!("{e:#}"), "replay: could not read the endpoint's model list");
                None
            }
        };
        let calls = match crate::replay::replay_items(&backend, &ep, model, &dataset).await {
            Ok(calls) => calls,
            Err(e) => {
                // The calls made (or refused) before the failure still go in the ledger.
                record_egress(data_dir, &backend.drain_egress(), &run_id).await;
                return Err(e);
            }
        };
        let mut spans = crate::replay::spans(&run_id, &ep, model, &calls);
        let summary = crate::replay::summary(&ep, model, provenance, &spans);
        spans.extend(crate::egress::spans(&backend.drain_egress(), &run_id));
        let candidates = crate::replay::candidates(&dataset, &calls, &run_id);
        if let Some(out) = out {
            let mut jsonl = String::new();
            for item in &candidates {
                jsonl.push_str(&serde_json::to_string(item)?);
                jsonl.push('\n');
            }
            std::fs::write(out, jsonl)
                .with_context(|| format!("writing the candidate dataset to {}", out.display()))?;
        }
        let run = RunInputs {
            config: &config,
            config_path,
            dataset_path: &dataset_path,
            dataset: &candidates,
            run_id,
        };
        let extra = Extra {
            spans,
            replay: Some(summary.clone()),
        };
        let report = evaluate_and_store(run, data_dir, fresh, extra).await?;
        crate::replay::print_summary(&summary);
        Ok(report.passed())
    }
}

/// What `judge probe --config` (and `doctor --judge-config`) probes: each endpoint the config's
/// judges call, once, with the first of its judges' models. `only` keeps one endpoint.
pub fn judge_probe_targets(
    config_path: &Path,
    only: Option<&str>,
) -> anyhow::Result<Vec<(crate::endpoint::Endpoint, String)>> {
    let config = load_config(config_path)?;
    let endpoints = crate::endpoint::resolve_all(&config.endpoints, &config_dir(config_path))?;
    let mut out: Vec<(crate::endpoint::Endpoint, String)> = Vec::new();
    for judge in &config.judges {
        judge.validate()?;
        let ep = judge.resolve_endpoint(&endpoints)?;
        if only.is_some_and(|name| name != ep.name) || out.iter().any(|(e, _)| e.name == ep.name) {
            continue;
        }
        out.push((ep, judge.model.clone()));
    }
    if let Some(name) = only {
        anyhow::ensure!(
            !out.is_empty(),
            "no judge in {} calls an endpoint named {name:?}",
            config_path.display()
        );
    }
    anyhow::ensure!(
        !out.is_empty(),
        "{} has no judges, so no endpoint to probe",
        config_path.display()
    );
    Ok(out)
}

/// The score name of a run's manifest.
pub const MANIFEST_NAME: &str = "evald.run.manifest";

/// The run manifest: one text score on the run (`{run_id}:manifest`) recording what decided its
/// scores, so a score can be traced to the evaluators, the judge endpoints and the weights they
/// reported, and the evald that ran them. Every per-item score's id starts with the run id.
fn manifest_score(
    config: &EvalConfig,
    dataset: &Path,
    items: usize,
    run_id: &str,
    ts_unix_nano: u64,
    judges: &[crate::judge::JudgeSummary],
    replay: Option<&crate::replay::ReplaySummary>,
) -> Score {
    let manifest = serde_json::json!({
        "manifest_version": 1,
        "evald_version": env!("CARGO_PKG_VERSION"),
        "run_id": run_id,
        "name": config.name,
        "dataset": dataset.display().to_string(),
        "items": items,
        "evaluators": config.evaluators.iter().map(|e| e.name().to_string()).collect::<Vec<_>>(),
        "judges": judges.iter().map(crate::judge::JudgeSummary::manifest_entry).collect::<Vec<_>>(),
        // A replay run's outputs came from this endpoint, not from the dataset.
        "replay": replay,
    });
    Score {
        id: format!("{run_id}:manifest"),
        target: ScoreTarget::Run(run_id.to_string()),
        name: MANIFEST_NAME.to_string(),
        num_value: None,
        str_value: Some(manifest.to_string()),
        data_type: crate::model::DataType::Text,
        source: crate::model::ScoreSource::Eval,
        comment: None,
        config_id: None,
        agg_stats: None,
        ts_unix_nano,
    }
}

/// A run's manifest ([`manifest_score`]), or `None` for a run written before manifests were.
pub fn run_manifest(store: &Store, run_id: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let id = format!("{run_id}:manifest");
    let scores = store
        .scores_for_target(&ScoreTarget::Run(run_id.to_string()))
        .map_err(|e| anyhow::anyhow!("reading scores for run {run_id}: {e}"))?;
    let Some(json) = scores
        .into_iter()
        .find(|s| s.id == id && s.name == MANIFEST_NAME)
        .and_then(|s| s.str_value)
    else {
        return Ok(None);
    };
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("run {run_id}: unreadable {MANIFEST_NAME} score: {e}"))
}

/// Something that decided a judge's scores and differs between two runs: its weights, the model
/// name it asked for, where it was served, or evald's judge prompts. A judge that changed makes
/// a delta the judge's as much as the system's.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JudgeChange {
    pub judge: String,
    /// `weights`, `quantization`, `model`, `base_url` or `judge_version`.
    pub what: &'static str,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// What changed about each judge the two manifests share. A judge in only one run is already a
/// `new` or `gone` row; a run without a manifest has nothing to compare.
pub fn judge_changes(a: &serde_json::Value, b: &serde_json::Value) -> Vec<JudgeChange> {
    let judges = |m: &serde_json::Value| -> BTreeMap<String, serde_json::Value> {
        m.get("judges")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|j| Some((j.get("name")?.as_str()?.to_string(), j.clone())))
            .collect()
    };
    let text = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Null) | None => None,
        Some(v) => Some(v.to_string()),
    };
    // The weights: the digest where the server gave one, else what it listed the model as.
    let weights = |j: &serde_json::Value| {
        text(j.pointer("/provenance/digest")).or_else(|| text(j.get("server_model")))
    };
    let (ja, jb) = (judges(a), judges(b));
    let mut out = Vec::new();
    for (name, x) in &ja {
        let Some(y) = jb.get(name) else { continue };
        let mut push = |what: &'static str, a: Option<String>, b: Option<String>| {
            if a != b {
                out.push(JudgeChange {
                    judge: name.clone(),
                    what,
                    a,
                    b,
                });
            }
        };
        push("weights", weights(x), weights(y));
        push(
            "quantization",
            text(x.pointer("/provenance/quantization")),
            text(y.pointer("/provenance/quantization")),
        );
        push("model", text(x.get("model")), text(y.get("model")));
        push("base_url", text(x.get("base_url")), text(y.get("base_url")));
        push(
            "judge_version",
            text(x.get("judge_version")),
            text(y.get("judge_version")),
        );
    }
    out
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
    let endpoints = crate::endpoint::resolve_all(&config.endpoints, &config_dir(config_path))?;
    let estimates = crate::judge::estimate_judges(&config.judges, &endpoints, &dataset, data_dir)?;
    print_estimate(&estimates, dataset.len());
    Ok(())
}

/// Print the judge cost estimate (indicative — see the disclaimer line).
fn print_estimate(estimates: &[crate::judge::JudgeEstimate], item_count: usize) {
    println!("\nevald eval — judge cost estimate ({item_count} item(s))");
    println!(
        "{:<22} {:<14} {:>8} {:>8} {:>12} {:>12} {:>12}",
        "judge", "endpoint", "to_call", "cached", "in_tokens", "out_tokens", "est_cost"
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
            "{:<22} {:<14} {:>8} {:>8} {:>12} {:>12} {:>12}",
            e.name, e.endpoint, e.calls, e.cached, e.input_tokens, e.output_tokens, cost
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
/// The directory a config's relative paths (dataset, an endpoint's certificate files) resolve
/// against.
fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

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

/// Store egress records of a run that did not finish, best-effort: the run's own error is what
/// the caller reports.
async fn record_egress(data_dir: &Path, records: &[crate::egress::Record], run_id: &str) {
    if records.is_empty() {
        return;
    }
    let store_config = StoreConfig {
        compact_interval: None,
        ..StoreConfig::default()
    };
    match Store::open(data_dir, store_config) {
        Ok(store) => {
            if let Err(e) = store
                .ingest_batch(crate::egress::spans(records, run_id), &[])
                .await
            {
                tracing::warn!(error = %e, "could not record the run's egress");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not open the store to record the run's egress"),
    }
}

/// The score name of a run's per-item values, one per evaluator (id `{run_id}:items:{name}`).
pub const ITEMS_NAME: &str = "evald.run.items";

/// One evaluator's per-item values in one run, as persisted: what `eval compare` pairs and
/// `eval calibrate --runs` measures stability over.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemValues {
    pub evaluator: String,
    /// `(item key, value)` for each scored item that has a key ([`DatasetItem::key`]), in
    /// dataset order.
    pub items: Vec<(String, f64)>,
    /// Scored items with no id, span or trace: counted, never paired.
    pub unkeyed: usize,
}

impl ItemValues {
    fn score(&self, run_id: &str, ts_unix_nano: u64) -> Score {
        Score {
            id: format!("{run_id}:items:{}", self.evaluator),
            target: ScoreTarget::Run(run_id.to_string()),
            name: ITEMS_NAME.to_string(),
            num_value: None,
            str_value: Some(serde_json::to_string(self).expect("plain data serializes")),
            data_type: DataType::Text,
            source: ScoreSource::Eval,
            comment: Some(format!(
                "per-item values of {} ({} keyed, {} without an id)",
                self.evaluator,
                self.items.len(),
                self.unkeyed
            )),
            config_id: None,
            agg_stats: None,
            ts_unix_nano,
        }
    }

    /// The values by key, or why they cannot be matched to another run's.
    pub fn by_key(&self) -> Result<BTreeMap<&str, f64>, String> {
        if self.unkeyed > 0 {
            return Err(format!(
                "{} item(s) have no id (give dataset items an `id` to pair runs)",
                self.unkeyed
            ));
        }
        let map: BTreeMap<&str, f64> = self.items.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        if map.len() != self.items.len() {
            return Err("item ids repeat within a run".to_string());
        }
        Ok(map)
    }
}

/// A run's per-item values by evaluator. Empty for a run written before they were recorded.
pub fn run_items(store: &Store, run_id: &str) -> anyhow::Result<BTreeMap<String, ItemValues>> {
    let prefix = format!("{run_id}:items:");
    let scores = store
        .scores_for_target(&ScoreTarget::Run(run_id.to_string()))
        .map_err(|e| anyhow::anyhow!("reading scores for run {run_id}: {e}"))?;
    let mut out = BTreeMap::new();
    for s in scores {
        if !s.id.starts_with(&prefix) || s.name != ITEMS_NAME {
            continue;
        }
        let Some(json) = s.str_value.as_deref() else {
            continue;
        };
        let v: ItemValues = serde_json::from_str(json).map_err(|e| {
            anyhow::anyhow!("run {run_id}: unreadable {ITEMS_NAME} score {}: {e}", s.id)
        })?;
        out.insert(v.evaluator.clone(), v);
    }
    Ok(out)
}

/// The per-item differences `b − a` when two runs scored exactly the same items, in key order;
/// otherwise why they cannot be paired.
pub fn paired_differences(
    a: Option<&ItemValues>,
    b: Option<&ItemValues>,
) -> Result<Vec<f64>, String> {
    let (Some(a), Some(b)) = (a, b) else {
        return Err("a run predates per-item values".to_string());
    };
    let (ka, kb) = (a.by_key()?, b.by_key()?);
    if ka.len() != kb.len() || ka.keys().any(|k| !kb.contains_key(k)) {
        let shared = ka.keys().filter(|k| kb.contains_key(*k)).count();
        return Err(format!(
            "the runs scored different items ({shared} shared of {} and {})",
            ka.len(),
            kb.len()
        ));
    }
    Ok(ka.iter().map(|(k, va)| kb[k] - va).collect())
}

/// [`paired_differences`] with each difference's item key, in key order.
pub fn paired_keyed<'v>(
    a: Option<&'v ItemValues>,
    b: Option<&'v ItemValues>,
) -> Result<Vec<(&'v str, f64)>, String> {
    let diffs = paired_differences(a, b)?;
    // Paired, so both are present and keyed alike; `by_key` iterates in the same order.
    let keys = a.expect("paired").by_key()?.into_keys();
    Ok(keys.zip(diffs).collect())
}

/// The score name of a run's slice labels (id `{run_id}:slices`).
pub const SLICES_NAME: &str = "evald.run.slices";

/// Each keyed item's label under each slice key a run recorded: `by[key][item] = value`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSlices {
    pub by: BTreeMap<String, BTreeMap<String, String>>,
}

impl RunSlices {
    /// The labels of `keys` in the dataset's item metadata. A string is taken as it is, a
    /// number or boolean as its text; anything else, or no key, leaves the item unlabelled.
    pub fn from_items(keys: &[String], dataset: &[DatasetItem]) -> RunSlices {
        let mut by: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for item in dataset {
            let (Some(key), Some(meta)) = (item.key(), item.metadata.as_ref()) else {
                continue;
            };
            for k in keys {
                let label = match meta.get(k) {
                    Some(serde_json::Value::String(v)) if !v.is_empty() => v.clone(),
                    Some(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => {
                        v.to_string()
                    }
                    _ => continue,
                };
                by.entry(k.clone())
                    .or_default()
                    .insert(key.to_string(), label);
            }
        }
        RunSlices { by }
    }

    fn score(&self, run_id: &str, ts_unix_nano: u64) -> Option<Score> {
        (!self.by.is_empty()).then(|| Score {
            id: format!("{run_id}:slices"),
            target: ScoreTarget::Run(run_id.to_string()),
            name: SLICES_NAME.to_string(),
            num_value: None,
            str_value: Some(serde_json::to_string(self).expect("plain data serializes")),
            data_type: DataType::Text,
            source: ScoreSource::Eval,
            comment: Some(format!(
                "item labels by {}",
                self.by.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
            config_id: None,
            agg_stats: None,
            ts_unix_nano,
        })
    }
}

/// A run's slice labels; empty for a run that recorded none.
pub fn run_slices(store: &Store, run_id: &str) -> anyhow::Result<RunSlices> {
    let id = format!("{run_id}:slices");
    let scores = store
        .scores_for_target(&ScoreTarget::Run(run_id.to_string()))
        .map_err(|e| anyhow::anyhow!("reading scores for run {run_id}: {e}"))?;
    match scores
        .into_iter()
        .find(|s| s.id == id && s.name == SLICES_NAME)
        .and_then(|s| s.str_value)
    {
        Some(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("run {run_id}: unreadable {SLICES_NAME} score: {e}")),
        None => Ok(RunSlices::default()),
    }
}

/// Which test a compare row's significance came from.
#[derive(Debug, Clone, PartialEq)]
pub enum TestUsed {
    /// The paired t-test on this many items scored in both runs.
    Paired { pairs: usize },
    /// Welch's two-sample test, and why the runs could not be paired.
    Welch { why: String },
}

impl TestUsed {
    pub fn name(&self) -> &'static str {
        match self {
            TestUsed::Paired { .. } => "paired",
            TestUsed::Welch { .. } => "welch",
        }
    }
}

/// The bootstrap's seed: fixed, so the same two runs always print the same interval.
const BOOTSTRAP_SEED: u64 = 0x0e7a_1d5e_ed00_0001;

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
    /// The t-test result: the p-value and the `(1 − alpha)` CI on `delta`.
    pub signif: Option<WelchResult>,
    /// Which test that was (set whenever both sides carried statistics).
    pub test: Option<TestUsed>,
    /// With the paired test, a percentile bootstrap `(1 − alpha)` CI on `delta`: the
    /// distribution-free check on the t interval.
    pub bootstrap_ci: Option<(f64, f64)>,
}

impl CompareRow {
    /// A numeric regression: present on both sides and B is worse than A by strictly more
    /// than `tolerance` (higher-is-better scorers). Evaluators missing on one side are
    /// *not* counted — the evaluator sets can legitimately differ across runs — but they
    /// are still surfaced in the table as `new`/`gone`.
    pub(crate) fn is_regression(&self, tolerance: f64) -> bool {
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
    pub(crate) fn gates_in_significance_mode(&self, tolerance: f64) -> bool {
        self.is_regression(tolerance) && !self.proven_noise()
    }
}

/// A run-vs-run aggregate comparison: one [`CompareRow`] per evaluator seen in either run.
#[derive(Debug, Clone)]
pub struct CompareReport {
    pub run_a: String,
    pub run_b: String,
    pub rows: Vec<CompareRow>,
    /// What changed about the judges between the runs ([`judge_changes`]).
    pub judge_changes: Vec<JudgeChange>,
    /// Each run's replay, when it was one ([`crate::replay::ReplaySummary`]): its cost and
    /// latency, set side by side.
    pub replay_a: Option<crate::replay::ReplaySummary>,
    pub replay_b: Option<crate::replay::ReplaySummary>,
    /// `--non-inferior`'s margin, and its verdicts ([`non_inferiority_rows`]).
    pub margin: Option<f64>,
    pub non_inferiority: Vec<NonInferiorRow>,
}

/// One `eval compare --non-inferior` verdict: an evaluator over every paired item, or over one
/// slice of them.
#[derive(Debug, Clone, PartialEq)]
pub struct NonInferiorRow {
    pub evaluator: String,
    /// `None`: every paired item. `Some((key, value))`: the items labelled `value` under `key`.
    pub slice: Option<(String, String)>,
    /// The test, or why it could not run.
    pub result: Result<crate::stats::NonInferiority, String>,
}

impl NonInferiorRow {
    pub fn non_inferior(&self) -> bool {
        matches!(&self.result, Ok(r) if r.verdict == crate::stats::NonInferiorVerdict::NonInferior)
    }

    /// `non-inferior`, `inferior`, `inconclusive`, or `untestable`.
    pub fn verdict(&self) -> &'static str {
        match &self.result {
            Ok(r) => r.verdict.as_str(),
            Err(_) => "untestable",
        }
    }

    pub fn slice_label(&self) -> String {
        match &self.slice {
            None => "all".to_string(),
            Some((k, v)) => format!("{k}={v}"),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let r = self.result.as_ref().ok();
        serde_json::json!({
            "evaluator": self.evaluator,
            "slice": self.slice.as_ref().map(|(k, v)| serde_json::json!({"key": k, "value": v})),
            "verdict": self.verdict(),
            "pairs": r.map(|r| r.pairs),
            "mean_delta": r.map(|r| r.mean),
            "lower_bound": r.map(|r| r.lower),
            "upper_bound": r.map(|r| r.upper),
            "p_value": r.map(|r| r.p),
            "why": self.result.as_ref().err(),
        })
    }
}

/// Is run B no worse than run A by more than `margin`, evaluator by evaluator? A one-sided
/// paired test on the items both runs scored ([`crate::stats::non_inferiority`] at level
/// `alpha`), over every item and then over each slice the runs recorded ([`RunSlices`]; run A's
/// labels, else run B's). Only evaluators in both runs are tested: one in a single run is
/// already a `new` or `gone` row.
pub fn non_inferiority_rows(
    store: &Store,
    run_a: &str,
    run_b: &str,
    margin: f64,
    alpha: f64,
) -> anyhow::Result<Vec<NonInferiorRow>> {
    use crate::stats::non_inferiority;
    let (items_a, items_b) = (run_items(store, run_a)?, run_items(store, run_b)?);
    let mut slices = run_slices(store, run_b)?;
    for (k, labels) in run_slices(store, run_a)?.by {
        slices.by.entry(k).or_default().extend(labels);
    }
    let test = |diffs: &[f64]| {
        non_inferiority(diffs, margin, alpha).ok_or_else(|| match diffs.len() {
            0 | 1 => format!("{} paired item(s), too few to test", diffs.len()),
            n => format!("all {n} items moved by the same amount: no variance to test"),
        })
    };
    let mut out = Vec::new();
    for (name, a) in &items_a {
        let Some(b) = items_b.get(name) else { continue };
        let keyed = match paired_keyed(Some(a), Some(b)) {
            Ok(k) => k,
            Err(why) => {
                out.push(NonInferiorRow {
                    evaluator: name.clone(),
                    slice: None,
                    result: Err(format!("needs the same items in both runs: {why}")),
                });
                continue;
            }
        };
        let diffs: Vec<f64> = keyed.iter().map(|(_, d)| *d).collect();
        out.push(NonInferiorRow {
            evaluator: name.clone(),
            slice: None,
            result: test(&diffs),
        });
        for (key, labels) in &slices.by {
            let mut by_value: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
            for (item, d) in &keyed {
                if let Some(v) = labels.get(*item) {
                    by_value.entry(v.as_str()).or_default().push(*d);
                }
            }
            for (value, diffs) in by_value {
                out.push(NonInferiorRow {
                    evaluator: name.clone(),
                    slice: Some((key.clone(), value.to_string())),
                    result: test(&diffs),
                });
            }
        }
    }
    Ok(out)
}

impl CompareReport {
    /// True if any shared evaluator regressed beyond `tolerance` — the default CI gate when
    /// `--fail-on-regression` is set (raw-delta, no significance test).
    pub fn has_regression(&self, tolerance: f64) -> bool {
        self.rows.iter().any(|r| r.is_regression(tolerance))
    }

    /// The `--non-inferior` gate: every shared evaluator, over all its paired items, shown no
    /// worse than run A by more than the margin. Slices are reported, not gated. False when no
    /// evaluator could be tested at all.
    pub fn non_inferior(&self) -> bool {
        let overall: Vec<&NonInferiorRow> = self
            .non_inferiority
            .iter()
            .filter(|r| r.slice.is_none())
            .collect();
        !overall.is_empty() && overall.iter().all(|r| r.non_inferior())
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
    let items_a = run_items(store, run_a)?;
    let items_b = run_items(store, run_b)?;
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
            // The same items in both runs: test the per-item differences, where the items'
            // own spread cancels. Otherwise Welch on the two runs' statistics, and say why.
            let (signif, test, bootstrap_ci) =
                match (ea.and_then(|e| e.stats), eb.and_then(|e| e.stats)) {
                    (Some(sa), Some(sb)) => {
                        match paired_differences(items_a.get(name), items_b.get(name)) {
                            Ok(diffs) => (
                                paired_t_test(&diffs, alpha),
                                Some(TestUsed::Paired { pairs: diffs.len() }),
                                bootstrap_mean_ci(
                                    &diffs,
                                    alpha,
                                    crate::stats::BOOTSTRAP_RESAMPLES,
                                    BOOTSTRAP_SEED,
                                ),
                            ),
                            Err(why) => (
                                welch_t_test(
                                    sa.mean,
                                    sa.variance,
                                    sa.n,
                                    sb.mean,
                                    sb.variance,
                                    sb.n,
                                    alpha,
                                ),
                                Some(TestUsed::Welch { why }),
                                None,
                            ),
                        }
                    }
                    _ => (None, None, None),
                };
            CompareRow {
                evaluator: name.clone(),
                a,
                b,
                delta,
                n_a,
                n_b,
                signif,
                test,
                bootstrap_ci,
            }
        })
        .collect();

    let (manifest_a, manifest_b) = (run_manifest(store, run_a)?, run_manifest(store, run_b)?);
    let judge_changes = match (&manifest_a, &manifest_b) {
        (Some(a), Some(b)) => judge_changes(a, b),
        _ => Vec::new(),
    };
    let replay = |m: &Option<serde_json::Value>| {
        m.as_ref()
            .and_then(|m| m.get("replay"))
            .filter(|r| !r.is_null())
            .and_then(|r| serde_json::from_value(r.clone()).ok())
    };
    Ok(CompareReport {
        run_a: run_a.to_string(),
        run_b: run_b.to_string(),
        rows,
        judge_changes,
        replay_a: replay(&manifest_a),
        replay_b: replay(&manifest_b),
        margin: None,
        non_inferiority: Vec::new(),
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
    let opts = CompareOpts {
        fail_on_regression,
        tolerance,
        significance,
        alpha,
        non_inferior_margin: None,
        reprice: false,
    };
    compare_command_junit(run_a, run_b, data_dir, opts, None, None).await
}

/// The gate settings of `eval compare`.
#[derive(Debug, Clone, Copy)]
pub struct CompareOpts {
    pub fail_on_regression: bool,
    pub tolerance: f64,
    pub significance: bool,
    pub alpha: f64,
    /// `--non-inferior --margin δ`: fail unless every shared evaluator is shown no worse than
    /// run A by more than δ.
    pub non_inferior_margin: Option<f64>,
    /// Price both runs' replays at the process price table (`--price-table`) rather than at
    /// what each replay recorded.
    pub reprice: bool,
}

/// [`compare_command`] that also writes a JUnit XML report to `junit` (see [`crate::junit`]),
/// one test case per evaluator delta, failing exactly when the gate fails. Written whether the
/// gate passes, fails, or the comparison errors; the verdict is what [`compare_command`] returns.
pub async fn compare_command_junit(
    run_a: &str,
    run_b: &str,
    data_dir: &Path,
    opts: CompareOpts,
    junit: Option<&Path>,
    report_json: Option<&Path>,
) -> anyhow::Result<bool> {
    let started = std::time::Instant::now();
    let result = compare_report(run_a, run_b, data_dir, opts).await;
    crate::junit::write_after(
        junit,
        &result,
        |(report, _)| {
            crate::junit::from_compare(
                report,
                opts.tolerance,
                opts.significance,
                opts.alpha,
                opts.fail_on_regression,
            )
        },
        &format!("evald eval compare {run_a} vs {run_b}"),
        "evald.eval.compare",
        started.elapsed().as_secs_f64(),
    )?;
    crate::report::write_after(
        report_json,
        &result,
        |(report, passed)| crate::report::compare(report, &opts, *passed),
        "eval_compare",
    )?;
    result.map(|(_, passed)| passed)
}

/// The body of `eval compare`: the report and whether the gate passes.
async fn compare_report(
    run_a: &str,
    run_b: &str,
    data_dir: &Path,
    opts: CompareOpts,
) -> anyhow::Result<(CompareReport, bool)> {
    let CompareOpts {
        fail_on_regression,
        tolerance,
        significance,
        alpha,
        non_inferior_margin,
        reprice,
    } = opts;
    let store_config = StoreConfig {
        compact_interval: None,
        ..StoreConfig::default()
    };
    let store = Store::open(data_dir, store_config)
        .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let mut report = compare_runs(&store, run_a, run_b, alpha)?;
    if reprice {
        let table = crate::price::current();
        for r in [&mut report.replay_a, &mut report.replay_b]
            .into_iter()
            .flatten()
        {
            r.reprice(&table);
        }
    }
    if let Some(margin) = non_inferior_margin {
        report.margin = Some(margin);
        report.non_inferiority = non_inferiority_rows(&store, run_a, run_b, margin, alpha)?;
    }
    print_compare(&report, tolerance, significance, alpha);

    let regressed = if significance {
        report.has_gating_significant_regression(tolerance)
    } else {
        report.has_regression(tolerance)
    };
    let regression_fails = fail_on_regression && regressed;
    if regression_fails {
        println!("--fail-on-regression set — exiting non-zero.");
    }
    let inferiority_fails = report.margin.is_some() && !report.non_inferior();
    if inferiority_fails {
        println!("--non-inferior set — exiting non-zero.");
    }
    Ok((report, !(regression_fails || inferiority_fails)))
}

/// The two runs' replays side by side: where they ran, what they cost, how long they took.
fn print_replays(report: &CompareReport) {
    use crate::replay::ReplaySummary;
    let (a, b) = (report.replay_a.as_ref(), report.replay_b.as_ref());
    if a.is_none() && b.is_none() {
        return;
    }
    let cell = |r: Option<&ReplaySummary>, f: fn(&ReplaySummary) -> String| {
        r.map(f).unwrap_or_else(|| "- (not a replay)".to_string())
    };
    let row = |name: &str, f: fn(&ReplaySummary) -> String, delta: String| {
        let line = format!("{name:<14} {:<30} {:<30} {delta}", cell(a, f), cell(b, f));
        println!("{}", line.trim_end());
    };
    let both = |f: fn(&ReplaySummary) -> Option<f64>| a.and_then(f).zip(b.and_then(f));
    let priced = |r: &ReplaySummary| r.cost_usd.filter(|_| r.unpriced_calls == 0);
    let ms_delta = |f: fn(&ReplaySummary) -> Option<f64>| {
        both(f).map_or("-".to_string(), |(x, y)| format!("{:+.0} ms", y - x))
    };

    println!("\n{:<14} {:<30} {:<30} delta", "replay", "run_a", "run_b");
    row(
        "endpoint",
        |r| format!("{} ({})", r.endpoint, r.model),
        String::new(),
    );
    row(
        "jurisdiction",
        |r| r.jurisdiction.clone().unwrap_or_else(|| "-".into()),
        String::new(),
    );
    row("calls", |r| r.calls.to_string(), String::new());
    row(
        "tokens in/out",
        |r| format!("{} / {}", r.input_tokens, r.output_tokens),
        String::new(),
    );
    row(
        "cost",
        |r| match r.cost_usd {
            Some(c) if r.unpriced_calls == 0 => format!("${c:.4}"),
            Some(c) => format!("${c:.4} +{} unpriced", r.unpriced_calls),
            None => "unpriced".to_string(),
        },
        both(priced).map_or("-".to_string(), |(x, y)| format!("{:+.4} $", y - x)),
    );
    row(
        "latency p50",
        |r| format!("{:.0} ms", r.latency_ms_p50),
        ms_delta(|r| Some(r.latency_ms_p50)),
    );
    row(
        "latency p95",
        |r| format!("{:.0} ms", r.latency_ms_p95),
        ms_delta(|r| Some(r.latency_ms_p95)),
    );
}

/// `--non-inferior`'s verdicts, every evaluator over all its items and then by slice.
fn print_non_inferiority(report: &CompareReport, alpha: f64) {
    let Some(margin) = report.margin else { return };
    println!(
        "\nnon-inferiority: is run B no worse than run A by more than {margin}? (paired, one-sided \
         {}% lower bound)",
        format!("{:.1}", (1.0 - alpha) * 100.0).trim_end_matches(".0")
    );
    println!(
        "{:<22} {:<20} {:>6} {:>9} {:>12}  verdict",
        "evaluator", "slice", "pairs", "mean Δ", "lower bound"
    );
    for r in &report.non_inferiority {
        match &r.result {
            Ok(t) => println!(
                "{:<22} {:<20} {:>6} {:>+9.3} {:>+12.4}  {}",
                r.evaluator,
                r.slice_label(),
                t.pairs,
                t.mean,
                t.lower,
                r.verdict()
            ),
            Err(why) => println!(
                "{:<22} {:<20} {:>6} {:>9} {:>12}  untestable: {why}",
                r.evaluator,
                r.slice_label(),
                "-",
                "-",
                "-"
            ),
        }
    }
    let failing: Vec<String> = report
        .non_inferiority
        .iter()
        .filter(|r| r.slice.is_none() && !r.non_inferior())
        .map(|r| format!("{} ({})", r.evaluator, r.verdict()))
        .collect();
    if report.non_inferior() {
        println!(
            "NON-INFERIOR — every evaluator is shown no worse than run A by more than {margin}."
        );
    } else if failing.is_empty() {
        println!("NOT SHOWN NON-INFERIOR — no evaluator was scored in both runs.");
    } else {
        println!(
            "NOT SHOWN NON-INFERIOR — {}. Inconclusive is not a pass: more items, or a wider \
             margin you can defend, decide it.",
            failing.join(", ")
        );
    }
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

    // Which test each verdict came from: detail under the table, not a column in it.
    if any_stats {
        println!();
        for r in &report.rows {
            let Some(test) = &r.test else { continue };
            let detail = match (test, r.signif, r.bootstrap_ci) {
                (TestUsed::Paired { pairs }, Some(_), Some((lo, hi))) => format!(
                    "paired t on {pairs} items scored in both runs; bootstrap {conf:.0}% CI \
                     [{lo:+.3},{hi:+.3}]"
                ),
                (TestUsed::Paired { pairs }, None, _) if *pairs >= 2 => format!(
                    "paired: all {pairs} items moved by the same amount, so there is no \
                     variance to test; the raw delta gates"
                ),
                (TestUsed::Paired { pairs }, _, _) => {
                    format!("paired: {pairs} item(s), too few to test")
                }
                (TestUsed::Welch { why }, _, _) => format!("Welch (unpaired): {why}"),
            };
            println!("  {:<14} {detail}", r.evaluator);
        }
    }

    if !report.judge_changes.is_empty() {
        println!(
            "\nwarning: a judge changed between the runs, so its deltas are the judge's as much as \
             the system's:"
        );
        let show = |v: &Option<String>| match v.as_deref() {
            Some(d) if d.starts_with("sha256:") && d.len() > 19 => format!("{}…", &d[..19]),
            Some(v) => v.to_string(),
            None => "-".to_string(),
        };
        for c in &report.judge_changes {
            println!(
                "  {:<14} {:<13} {} -> {}",
                c.judge,
                c.what,
                show(&c.a),
                show(&c.b)
            );
        }
    }

    print_replays(report);
    print_non_inferiority(report, alpha);

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
            ExactMatch { normalize: false },
            ExactMatch { normalize: true },
            Contains {
                substring: Some("Par".into()),
            },
            Regex {
                pattern: "P.*s".into(),
            },
            JsonValid,
            Levenshtein {
                threshold: 0.8,
                normalize: true,
            },
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
            LanguageMatch {
                language: Some("en".into()),
            },
            Script {
                script: Some("Latin".into()),
                min: 0.8,
            },
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
            ExactMatch { normalize: false }
                .score(&item("Paris", Some("Paris")))
                .unwrap()
                .value,
            1.0
        );
        assert_eq!(
            ExactMatch { normalize: false }
                .score(&item(" Paris ", Some("Paris")))
                .unwrap()
                .value,
            1.0
        ); // trimmed
        assert_eq!(
            ExactMatch { normalize: false }
                .score(&item("4", Some("four")))
                .unwrap()
                .value,
            0.0
        );
        assert!(ExactMatch { normalize: false }
            .score(&item("x", None))
            .is_none()); // no expected -> skipped

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

        let l = Levenshtein {
            threshold: 0.8,
            normalize: false,
        };
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
            evaluators: vec![EvaluatorSpec::ExactMatch { normalize: false }],
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::from([("exact_match".to_string(), 0.9)]),
            slice_by: None,
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
        // No spans on the items, so nothing attaches per item: the run gets the aggregate and
        // the per-item values (none keyed, the items have no id).
        assert_eq!(scores.len(), 2);
        assert!(scores
            .iter()
            .all(|s| matches!(s.target, ScoreTarget::Run(_))));
        let agg_score = scores
            .iter()
            .find(|s| s.id == "run1:agg:exact_match")
            .unwrap();
        assert_eq!(agg_score.num_value, Some(0.75));
        let items = scores.iter().find(|s| s.name == ITEMS_NAME).unwrap();
        assert_eq!(items.id, "run1:items:exact_match");
        assert_eq!(items.num_value, None, "never mistaken for a numeric score");
        let v: ItemValues = serde_json::from_str(items.str_value.as_deref().unwrap()).unwrap();
        assert_eq!((v.items.len(), v.unkeyed), (0, 4));
    }

    #[test]
    fn run_eval_attaches_per_item_scores_to_spans() {
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::ExactMatch { normalize: false }],
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
        };
        let mut a = item("a", Some("a"));
        a.span_id = Some("span-1".into());
        let (_report, scores) = run_eval(&config, &[a], "run2", 1).unwrap();
        // one per-item score (targeting the span) + the per-item values and the aggregate
        // (targeting the run)
        assert_eq!(scores.len(), 3);
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
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
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
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
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
            evaluators: vec![EvaluatorSpec::ExactMatch { normalize: false }],
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::from([("exact_match".to_string(), 0.9)]),
            slice_by: None,
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
        // The aggregate Score is still persisted (mean 0.0 over 0 items) for the record, beside
        // its (empty) per-item values.
        assert_eq!(scores.len(), 2);
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

    /// The JUnit report is written when the gate FAILS (that is when CI needs it) and the returned
    /// verdict is exactly what the plain command returns.
    #[tokio::test]
    async fn run_command_junit_is_written_on_a_failing_gate_and_the_verdict_is_unchanged() {
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
        let junit = dir.path().join("reports/eval.xml");
        let passed = run_command_junit(
            &cfg_path,
            &dir.path().join("store"),
            Some(&junit),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!passed, "0.5 < 0.9: the gate still fails");
        let xml = std::fs::read_to_string(&junit).unwrap();
        assert!(
            xml.contains("tests=\"1\" failures=\"1\" errors=\"0\" skipped=\"0\""),
            "{xml}"
        );
        assert!(xml.contains("type=\"ThresholdNotMet\""));
        assert!(xml.contains("name=\"exact_match\""));
    }

    #[tokio::test]
    async fn run_command_junit_reports_a_command_that_could_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let junit = dir.path().join("eval.xml");
        let err = run_command_junit(
            &dir.path().join("missing.yaml"),
            &dir.path().join("s"),
            Some(&junit),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "the original error still propagates"
        );
        let xml = std::fs::read_to_string(&junit).unwrap();
        assert!(
            xml.contains("tests=\"1\" failures=\"0\" errors=\"1\" skipped=\"0\""),
            "{xml}"
        );
    }

    #[tokio::test]
    async fn compare_command_junit_fails_exactly_when_the_gate_does() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            persist_run(
                &store,
                vec![EvaluatorSpec::ExactMatch { normalize: false }],
                &[item("a", Some("a")), item("b", Some("b"))],
                "good",
            );
            persist_run(
                &store,
                vec![EvaluatorSpec::ExactMatch { normalize: false }],
                &[item("a", Some("a")), item("X", Some("b"))],
                "bad",
            );
        }
        let opts = |fail_on_regression| CompareOpts {
            fail_on_regression,
            tolerance: 0.0,
            significance: false,
            alpha: 0.05,
            non_inferior_margin: None,
            reprice: false,
        };
        let junit = dir.path().join("cmp.xml");
        let passed =
            compare_command_junit("good", "bad", &data_dir, opts(true), Some(&junit), None)
                .await
                .unwrap();
        assert!(!passed);
        let xml = std::fs::read_to_string(&junit).unwrap();
        assert!(
            xml.contains("failures=\"1\"") && xml.contains("type=\"Regression\""),
            "{xml}"
        );
        // Informational compare: the same regression is reported but nothing fails.
        let passed =
            compare_command_junit("good", "bad", &data_dir, opts(false), Some(&junit), None)
                .await
                .unwrap();
        assert!(passed);
        assert!(std::fs::read_to_string(&junit)
            .unwrap()
            .contains("failures=\"0\""));
        // An unknown run id is an errored report, and the error still propagates.
        assert!(
            compare_command_junit("good", "ghost", &data_dir, opts(true), Some(&junit), None)
                .await
                .is_err()
        );
        assert!(std::fs::read_to_string(&junit)
            .unwrap()
            .contains("errors=\"1\""));
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
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
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
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
            &[item("a", Some("a")), item("X", Some("b"))],
            "runA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
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
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
            &[item("a", Some("a")), item("b", Some("b"))],
            "rA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
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
            vec![
                EvaluatorSpec::ExactMatch { normalize: false },
                EvaluatorSpec::JsonValid,
            ],
            &[item("{}", Some("{}"))],
            "uA",
        );
        persist_run(
            &store,
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
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
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
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
                vec![EvaluatorSpec::ExactMatch { normalize: false }],
                &[item("a", Some("a")), item("b", Some("b"))],
                "good",
            );
            persist_run(
                &store,
                vec![EvaluatorSpec::ExactMatch { normalize: false }],
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
        persist_run(
            store,
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
            &items,
            run_id,
        );
    }

    /// `(item id, passes)` per item of an exact_match run.
    type Outcomes = Vec<(Option<String>, bool)>;

    /// A run of exact_match over `outcomes`.
    fn persist_items(store: &Store, run_id: &str, outcomes: &[(Option<String>, bool)]) {
        let items: Vec<DatasetItem> = outcomes
            .iter()
            .map(|(id, pass)| DatasetItem {
                id: id.clone(),
                ..item(if *pass { "a" } else { "x" }, Some("a"))
            })
            .collect();
        persist_run(
            store,
            vec![EvaluatorSpec::ExactMatch { normalize: false }],
            &items,
            run_id,
        );
    }

    /// 40 items, the first 20 passing in run A; in run B five of those 20 fail. Every other
    /// item scores the same in both runs.
    fn five_flips(ids: bool) -> (Outcomes, Outcomes) {
        let id = |i: usize| ids.then(|| format!("q{i}"));
        let a = (0..40).map(|i| (id(i), i < 20)).collect();
        let b = (0..40).map(|i| (id(i), i < 15)).collect();
        (a, b)
    }

    #[tokio::test]
    async fn compare_pairs_the_same_items_and_sees_what_welch_calls_noise() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (a, b) = five_flips(true);
        persist_items(&store, "pA", &a);
        persist_items(&store, "pB", &b);
        let paired = compare_runs(&store, "pA", "pB", 0.05).unwrap();
        let row = &paired.rows[0];
        assert_eq!(row.test, Some(TestUsed::Paired { pairs: 40 }));
        let s = row.signif.unwrap();
        // By hand: d = five −1s and 35 zeros, d̄ = −0.125, s_d² = (5·0.875² + 35·0.125²) / 39,
        // t = d̄ / (s_d / √40) ≈ −2.36 on 39 df.
        let var = (5.0 * 0.875_f64.powi(2) + 35.0 * 0.125_f64.powi(2)) / 39.0;
        assert!(
            (s.t - (-0.125 / (var / 40.0).sqrt())).abs() < 1e-9,
            "t={}",
            s.t
        );
        assert_eq!(s.df, 39.0);
        assert!(
            s.ci_high < 0.0,
            "paired CI excludes 0: {:?}",
            (s.ci_low, s.ci_high)
        );
        let (lo, hi) = row.bootstrap_ci.unwrap();
        assert!(lo <= -0.125 && (-0.125..0.0).contains(&hi), "({lo}, {hi})");
        assert!(paired.has_gating_significant_regression(0.0));

        // The same scores with no item ids cannot be paired: Welch, which calls it noise.
        let (a, b) = five_flips(false);
        persist_items(&store, "uA", &a);
        persist_items(&store, "uB", &b);
        let unpaired = compare_runs(&store, "uA", "uB", 0.05).unwrap();
        let row = &unpaired.rows[0];
        match &row.test {
            Some(TestUsed::Welch { why }) => assert!(why.contains("no id"), "{why}"),
            other => panic!("{other:?}"),
        }
        let s = row.signif.unwrap();
        assert!(
            s.ci_low < 0.0 && s.ci_high > 0.0,
            "{:?}",
            (s.ci_low, s.ci_high)
        );
        assert!(row.bootstrap_ci.is_none());
        assert!(!unpaired.has_gating_significant_regression(0.0));
        assert_eq!(row.delta, paired.rows[0].delta, "the same delta either way");
    }

    #[tokio::test]
    async fn compare_says_why_runs_could_not_be_paired() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (a, mut b) = five_flips(true);
        b.pop(); // run B scored one item fewer
        persist_items(&store, "dA", &a);
        persist_items(&store, "dB", &b);
        let r = compare_runs(&store, "dA", "dB", 0.05).unwrap();
        assert_eq!(
            r.rows[0].test,
            Some(TestUsed::Welch {
                why: "the runs scored different items (39 shared of 40 and 39)".into()
            })
        );
        // Repeated ids cannot be matched either.
        let dup: Outcomes = (0..4).map(|i| (Some("same".to_string()), i < 2)).collect();
        persist_items(&store, "rA", &dup);
        persist_items(&store, "rB", &dup);
        let r = compare_runs(&store, "rA", "rB", 0.05).unwrap();
        assert!(matches!(&r.rows[0].test, Some(TestUsed::Welch { why }) if why.contains("repeat")));
    }

    /// 100 items, 60 labelled `en` and 40 `ms`; run A passes all of them, run B fails `ms_fails`
    /// of the `ms` ones. Both runs record their slice labels.
    fn persist_migration(store: &Store, ms_fails: usize) {
        let items = |fails: usize| -> Vec<DatasetItem> {
            (0..100)
                .map(|i| {
                    let ms = i >= 60;
                    let fail = ms && i - 60 < fails;
                    DatasetItem {
                        id: Some(format!("q{i}")),
                        metadata: Some(
                            serde_json::json!({"language": if ms { "ms" } else { "en" }}),
                        ),
                        ..item(if fail { "x" } else { "a" }, Some("a"))
                    }
                })
                .collect()
        };
        let keys = vec!["language".to_string()];
        for (run, fails) in [("mA", 0), ("mB", ms_fails)] {
            let dataset = items(fails);
            persist_run(
                store,
                vec![EvaluatorSpec::ExactMatch { normalize: false }],
                &dataset,
                run,
            );
            store
                .put_scores(&[RunSlices::from_items(&keys, &dataset)
                    .score(run, 1)
                    .unwrap()])
                .unwrap();
        }
    }

    #[tokio::test]
    async fn non_inferiority_by_slice_known_answers() {
        use crate::stats::NonInferiorVerdict::*;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        persist_migration(&store, 6);
        let verdicts = |margin: f64| -> Vec<(String, Option<crate::stats::NonInferiorVerdict>)> {
            non_inferiority_rows(&store, "mA", "mB", margin, 0.05)
                .unwrap()
                .iter()
                .map(|r| (r.slice_label(), r.result.as_ref().ok().map(|t| t.verdict)))
                .collect()
        };
        // Overall: 6 of 100 items worse, mean −0.06, one-sided 95% bounds [−0.0996, −0.0204]:
        // neither shown at a 0.05 margin. The `ms` slice: 6 of 40, mean −0.15, upper bound
        // −0.0537, so shown worse than 0.05. `en`: unchanged on every item.
        assert_eq!(
            verdicts(0.05),
            [
                ("all".to_string(), Some(Inconclusive)),
                ("language=en".to_string(), Some(NonInferior)),
                ("language=ms".to_string(), Some(Inferior)),
            ]
        );
        let rows = non_inferiority_rows(&store, "mA", "mB", 0.05, 0.05).unwrap();
        let all = rows[0].result.as_ref().unwrap();
        assert_eq!(all.pairs, 100);
        assert!((all.mean + 0.06).abs() < 1e-12);
        assert!((all.upper + 0.020369).abs() < 1e-5, "{all:?}");
        let ms = rows[2].result.as_ref().unwrap();
        assert_eq!(ms.pairs, 40);
        assert!((ms.upper + 0.053664).abs() < 1e-5, "{ms:?}");
        // A margin of 0.12 accepts the overall drop (lower bound −0.0996), not the ms slice's.
        assert_eq!(verdicts(0.12)[0].1, Some(NonInferior));
        assert_eq!(verdicts(0.12)[2].1, Some(Inconclusive));

        // Runs over different items cannot be paired, and the row says so.
        persist_items(
            &store,
            "other",
            &[(Some("q0".into()), true), (Some("zz".into()), true)],
        );
        let r = non_inferiority_rows(&store, "mA", "other", 0.05, 0.05).unwrap();
        assert_eq!(r.len(), 1);
        assert!(
            matches!(&r[0].result, Err(w) if w.contains("same items")),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn compare_gates_on_non_inferiority_and_inconclusive_fails() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            persist_migration(&store, 6);
        }
        let opts = |margin: Option<f64>| CompareOpts {
            fail_on_regression: false,
            tolerance: 0.0,
            significance: false,
            alpha: 0.05,
            non_inferior_margin: margin,
            reprice: false,
        };
        let junit = dir.path().join("ni.xml");
        // Inconclusive at 0.05: not a pass.
        let passed =
            compare_command_junit("mA", "mB", &data_dir, opts(Some(0.05)), Some(&junit), None)
                .await
                .unwrap();
        assert!(!passed);
        let xml = std::fs::read_to_string(&junit).unwrap();
        assert!(xml.contains("non-inferior: exact_match"), "{xml}");
        assert!(xml.contains("NotNonInferior"), "{xml}");
        assert!(xml.contains("non_inferior_margin"), "{xml}");
        // Shown non-inferior at 0.12 (the ms slice is reported, not gated).
        let passed =
            compare_command_junit("mA", "mB", &data_dir, opts(Some(0.12)), Some(&junit), None)
                .await
                .unwrap();
        assert!(passed);
        assert!(!std::fs::read_to_string(&junit)
            .unwrap()
            .contains("NotNonInferior"));
        // Without --non-inferior nothing gates (no --fail-on-regression either).
        assert!(
            compare_command_junit("mA", "mB", &data_dir, opts(None), None, None)
                .await
                .unwrap()
        );
    }

    /// Every report `--report-json` writes validates against the published v1 schema: a run,
    /// a comparison with every section filled, and a command that could not run.
    #[tokio::test]
    async fn reports_validate_against_the_v1_schema() {
        let schema: serde_json::Value = serde_json::from_str(crate::report::SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let check = |doc: &serde_json::Value| {
            let errors: Vec<String> = validator.iter_errors(doc).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{errors:#?}\n{doc:#}");
        };
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("store");
        {
            let store = test_store(&data_dir);
            persist_migration(&store, 6);
            put_manifest(&store, "mA", vec![judge_entry("sha256:aaa", "Q4_K_M")]);
            put_manifest(&store, "mB", vec![judge_entry("sha256:bbb", "Q4_K_M")]);
        }
        let opts = CompareOpts {
            fail_on_regression: true,
            tolerance: 0.0,
            significance: true,
            alpha: 0.05,
            non_inferior_margin: Some(0.05),
            reprice: false,
        };
        let out = dir.path().join("cmp.json");
        let passed = compare_command_junit("mA", "mB", &data_dir, opts, None, Some(&out))
            .await
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        check(&doc);
        assert_eq!(doc["schema"], crate::report::VERSION);
        assert_eq!(doc["passed"], passed);
        assert_eq!(doc["judge_changes"][0]["what"], "weights");
        assert_eq!(doc["non_inferiority"][0]["verdict"], "inconclusive");
        assert_eq!(doc["evaluators"][0]["test"], "paired");

        // A comparison that could not run still writes a report, and it validates.
        let missing = dir.path().join("missing.json");
        compare_command_junit("mA", "ghost", &data_dir, opts, None, Some(&missing))
            .await
            .unwrap_err();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&missing).unwrap()).unwrap();
        check(&doc);
        assert_eq!(doc["passed"], false);
        assert!(doc["error"].as_str().unwrap().contains("ghost"));

        // An eval run, with slices.
        let cfg = dir.path().join("eval.yaml");
        std::fs::write(
            dir.path().join("d.jsonl"),
            "{\"id\":\"q1\",\"output\":\"a\",\"expected_output\":\"a\",\"metadata\":{\"language\":\"en\"}}\n\
             {\"id\":\"q2\",\"output\":\"x\",\"expected_output\":\"a\",\"metadata\":{\"language\":\"ms\"}}\n",
        )
        .unwrap();
        std::fs::write(
            &cfg,
            "dataset: ./d.jsonl\nevaluators:\n  - type: exact_match\nthresholds:\n  exact_match: 0.9\n",
        )
        .unwrap();
        let run_json = dir.path().join("run.json");
        let passed = run_command_junit(&cfg, &dir.path().join("s2"), None, Some(&run_json), false)
            .await
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&run_json).unwrap()).unwrap();
        check(&doc);
        assert!(!passed);
        assert_eq!(doc["kind"], "eval_run");
        assert_eq!(doc["evaluators"][0]["threshold_met"], false);
        assert_eq!(doc["slices"].as_array().unwrap().len(), 2);

        // And the schema has teeth: a report missing what v1 promises does not validate.
        let mut broken = doc.clone();
        broken.as_object_mut().unwrap().remove("evaluators");
        assert!(!validator.is_valid(&broken));
    }

    #[test]
    fn multilingual_evaluators_from_yaml() {
        let specs: Vec<EvaluatorSpec> = serde_yaml::from_str(
            "- type: exact_match\n  normalize: true\n- type: levenshtein\n  normalize: true\n\
             - type: language_match\n- type: script\n- type: script\n  script: Jawi\n  min: 0.9\n",
        )
        .unwrap();
        let ev: Vec<Box<dyn Evaluator>> = specs.iter().map(|s| s.compile().unwrap()).collect();
        let it = |out: &str, expected: Option<&str>, lang: Option<&str>| DatasetItem {
            metadata: lang.map(|l| serde_json::json!({"language": l})),
            ..item(out, expected)
        };
        // Full-width digits and case: equal once normalised, not before.
        let wide = it("ＫＬ２０２６", Some("kl2026"), None);
        assert_eq!(ev[0].score(&wide).unwrap().value, 1.0);
        assert_eq!(
            EvaluatorSpec::ExactMatch { normalize: false }
                .evaluate(&wide)
                .unwrap()
                .unwrap()
                .value,
            0.0
        );
        assert_eq!(ev[1].score(&wide).unwrap().value, 1.0);

        // Asked in Malay, answered in English: the failure language_match exists for.
        let english = it(
            "Your order has been shipped and should arrive within three working days.",
            None,
            Some("ms"),
        );
        let s = ev[2].score(&english).unwrap();
        assert!(!s.passed);
        assert!(s.comment.unwrap().contains("detected en"));
        let malay = it(
            "Pesanan anda telah dihantar dan akan tiba dalam masa tiga hari bekerja.",
            None,
            Some("id"),
        );
        assert!(ev[2].score(&malay).unwrap().passed, "ms and id are one");
        assert!(
            ev[2].score(&it("OK", None, Some("ms"))).is_none(),
            "too short: skipped"
        );
        assert!(
            ev[2]
                .score(&it("anything at all here", None, None))
                .is_none(),
            "no language"
        );
        // A configured language wins over the item's.
        let fixed = EvaluatorSpec::LanguageMatch {
            language: Some("en".into()),
        };
        assert!(fixed.evaluate(&english).unwrap().unwrap().passed);

        // The script of the item's language; or one named (Jawi is Arabic script).
        let thai = it("ราคา 39,900 บาท", None, Some("th"));
        assert_eq!(ev[3].score(&thai).unwrap().value, 1.0);
        let latin_for_thai = it("The price is 39,900 baht", None, Some("th"));
        let s = ev[3].score(&latin_for_thai).unwrap();
        assert!(!s.passed && s.value == 0.0, "{s:?}");
        assert!(
            ev[3].score(&it("text", None, Some("tlh"))).is_none(),
            "no known script"
        );
        let jawi = it("بهاس ملايو", None, None);
        assert!(ev[4].score(&jawi).unwrap().passed);
        assert!(
            !ev[4]
                .score(&it("Bahasa Melayu", None, None))
                .unwrap()
                .passed
        );

        // Bad parameters are refused when the config loads.
        for bad in [
            "- type: script\n  script: Klingon\n",
            "- type: script\n  min: 2\n",
        ] {
            let specs: Vec<EvaluatorSpec> = serde_yaml::from_str(bad).unwrap();
            assert!(specs[0].compile().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_run_reports_each_evaluator_by_language() {
        let dataset: Vec<DatasetItem> = [("q1", "en", "a"), ("q2", "en", "x"), ("q3", "ms", "a")]
            .iter()
            .map(|(id, lang, out)| DatasetItem {
                id: Some(id.to_string()),
                metadata: Some(serde_json::json!({"language": lang, "route": "faq"})),
                ..item(out, Some("a"))
            })
            .collect();
        let config = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![EvaluatorSpec::ExactMatch { normalize: false }],
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
        };
        let (_, scores) = run_eval(&config, &dataset, "r", 1).unwrap();
        let slices = RunSlices::from_items(&config.slice_keys(), &dataset);
        let rows = slice_means(&scores, &slices);
        // `route` has one value, so says nothing per slice; `language` has two.
        assert_eq!(
            rows.iter()
                .map(|r| (r.key.as_str(), r.value.as_str(), r.n, r.mean))
                .collect::<Vec<_>>(),
            [("language", "en", 2, 0.5), ("language", "ms", 1, 1.0)]
        );
    }

    #[test]
    fn slice_labels_come_from_item_metadata() {
        let it = |id: &str, meta: serde_json::Value| DatasetItem {
            id: Some(id.into()),
            metadata: Some(meta),
            ..item("o", None)
        };
        let items = [
            it(
                "a",
                serde_json::json!({"language": "ms", "route": 3, "task": ""}),
            ),
            it(
                "b",
                serde_json::json!({"language": "en", "task": {"nested": 1}}),
            ),
            DatasetItem {
                metadata: Some(serde_json::json!({"language": "ms"})),
                ..item("unkeyed", None)
            },
        ];
        let keys = EvalConfig {
            name: None,
            dataset: "d.jsonl".into(),
            evaluators: vec![],
            endpoints: vec![],
            judges: vec![],
            thresholds: BTreeMap::new(),
            slice_by: None,
        }
        .slice_keys();
        assert_eq!(keys, ["language", "route", "task"]);
        let s = RunSlices::from_items(&keys, &items);
        assert_eq!(
            s.by["language"].len(),
            2,
            "an unkeyed item cannot be paired"
        );
        assert_eq!(s.by["language"]["a"], "ms");
        assert_eq!(s.by["route"]["a"], "3", "a number is its text");
        assert!(
            !s.by.contains_key("task"),
            "empty and nested values label nothing"
        );
        assert!(RunSlices::from_items(&keys, &[]).score("r", 1).is_none());
    }

    /// A judge's manifest entry, as [`crate::judge::JudgeSummary::manifest_entry`] writes it.
    fn judge_entry(digest: &str, quant: &str) -> serde_json::Value {
        serde_json::json!({
            "name": "judge_toxicity",
            "judge_version": crate::judge::JUDGE_VERSION,
            "base_url": "http://ollama:11434",
            "model": "qwen2.5:7b",
            "server_model": digest,
            "provenance": {"id": "qwen2.5:7b", "digest": digest, "quantization": quant,
                           "source": "v1/models+ollama"},
        })
    }

    fn put_manifest(store: &Store, run_id: &str, judges: Vec<serde_json::Value>) {
        let m = serde_json::json!({"manifest_version": 1, "run_id": run_id, "judges": judges});
        store
            .put_scores(&[Score {
                id: format!("{run_id}:manifest"),
                target: ScoreTarget::Run(run_id.to_string()),
                name: MANIFEST_NAME.to_string(),
                num_value: None,
                str_value: Some(m.to_string()),
                data_type: crate::model::DataType::Text,
                source: crate::model::ScoreSource::Eval,
                comment: None,
                config_id: None,
                agg_stats: None,
                ts_unix_nano: 1,
            }])
            .unwrap();
    }

    #[test]
    fn a_judge_that_changed_between_runs_is_named_with_what_changed() {
        let a = serde_json::json!({"judges": [judge_entry("sha256:aaa", "Q4_K_M")]});
        assert!(
            judge_changes(&a, &a).is_empty(),
            "the same judge: nothing to say"
        );

        // Re-pulled under the same tag: new weights, new quantisation.
        let b = serde_json::json!({"judges": [judge_entry("sha256:bbb", "Q8_0")]});
        let c = judge_changes(&a, &b);
        assert_eq!(
            c.iter().map(|c| c.what).collect::<Vec<_>>(),
            ["weights", "quantization"]
        );
        assert_eq!(c[0].judge, "judge_toxicity");
        assert_eq!(
            (c[0].a.as_deref(), c[0].b.as_deref()),
            (Some("sha256:aaa"), Some("sha256:bbb"))
        );

        // Moved, renamed, or judged with other prompts: each is named.
        let mut moved = judge_entry("sha256:aaa", "Q4_K_M");
        moved["base_url"] = "http://gpu-2:8000".into();
        moved["model"] = "qwen2.5:14b".into();
        moved["judge_version"] = "0".into();
        let c = judge_changes(&a, &serde_json::json!({"judges": [moved]}));
        assert_eq!(
            c.iter().map(|c| c.what).collect::<Vec<_>>(),
            ["model", "base_url", "judge_version"]
        );

        // Without a digest (vLLM, a gateway), the listed model stands for the weights.
        let listed = |id: &str| {
            serde_json::json!({"judges": [{"name": "j", "server_model": id,
                                           "provenance": {"id": id, "source": "v1/models"}}]})
        };
        let c = judge_changes(&listed("/models/a"), &listed("/models/b"));
        assert_eq!((c.len(), c[0].what), (1, "weights"));

        // A judge in only one run, or a run with no manifest judges: nothing to compare.
        let other = serde_json::json!({"judges": [{"name": "other", "server_model": "x"}]});
        assert!(judge_changes(&a, &other).is_empty());
        assert!(judge_changes(&a, &serde_json::json!({})).is_empty());
    }

    #[tokio::test]
    async fn compare_warns_when_the_judge_changed() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let (a, b) = five_flips(true);
        persist_items(&store, "jA", &a);
        persist_items(&store, "jB", &b);
        // No manifests (runs written before 0.5, or with no judges): no warning.
        assert!(compare_runs(&store, "jA", "jB", 0.05)
            .unwrap()
            .judge_changes
            .is_empty());
        put_manifest(&store, "jA", vec![judge_entry("sha256:aaa", "Q4_K_M")]);
        put_manifest(&store, "jB", vec![judge_entry("sha256:bbb", "Q4_K_M")]);
        let r = compare_runs(&store, "jA", "jB", 0.05).unwrap();
        assert_eq!(r.judge_changes.len(), 1, "{:?}", r.judge_changes);
        assert_eq!(r.judge_changes[0].what, "weights");
        assert_eq!(run_manifest(&store, "jA").unwrap().unwrap()["run_id"], "jA");
        assert!(run_manifest(&store, "no-such-run").unwrap().is_none());
    }

    /// Every eval config docs/JUDGES.md shows, and the Ollama stack's, loads and resolves as
    /// written: a page that tells you to paste a config must paste one evald takes.
    #[test]
    fn the_judges_page_and_the_stack_configs_load() {
        let page = include_str!("../docs/JUDGES.md");
        let mut configs: Vec<&str> = page
            .split("```yaml\n")
            .skip(1)
            .map(|b| b.split("```").next().unwrap())
            .collect();
        assert_eq!(configs.len(), 2, "the page's two judge configs");
        configs.push(include_str!("../ops/stacks/ollama/judge.yaml"));
        for text in configs {
            let cfg: EvalConfig =
                serde_yaml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
            let endpoints = crate::endpoint::resolve_all(&cfg.endpoints, Path::new(".")).unwrap();
            assert!(!cfg.judges.is_empty());
            for j in &cfg.judges {
                j.validate().unwrap();
                j.resolve_endpoint(&endpoints).unwrap();
            }
        }
        let online: crate::online::OnlineSpec =
            serde_yaml::from_str(include_str!("../ops/stacks/ollama/online.yaml")).unwrap();
        crate::online::Online::new(&online, Path::new(".")).unwrap();
    }

    #[test]
    fn an_item_is_known_by_its_id_then_its_span_then_its_trace() {
        let mut it = item("o", None);
        assert_eq!(it.key(), None);
        it.trace_id = Some("t".into());
        assert_eq!(it.key(), Some("t"));
        it.span_id = Some("s".into());
        assert_eq!(it.key(), Some("s"));
        it.id = Some(String::new());
        assert_eq!(it.key(), Some("s"), "an empty id is no id");
        it.id = Some("q1".into());
        assert_eq!(it.key(), Some("q1"));
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
