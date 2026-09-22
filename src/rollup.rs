//! Score rollup — what a *trace* scores when its spans are scored.
//!
//! `PLAN.md` §2.3 has carried this as an open model gap since the first revision: a score
//! attaches to a span, but an agentic task emits a multi-span trace with no single canonical
//! span to hang the answer on, and "the trace's faithfulness" was therefore **undefined**.
//! Undefined is worse than wrong: `store.scores_for_target(Trace(id))` returns nothing at
//! all for a trace whose every span is scored, so run-compare and online eval silently see
//! an empty set rather than a number they could argue with.
//!
//! This module defines it. Four rules:
//!
//! ## 1. Writing is unchanged; the question is asked at read time
//!
//! A score on a span means that span. A score on a trace means the trace. Nothing is
//! promoted at write time, so no existing score changes meaning and there is nothing to
//! migrate. Rollup happens when someone *asks* for a trace's score, over whatever is stored
//! at that moment — which also means a score attached later is picked up with no rebuild.
//!
//! ## 2. Measured beats derived, always
//!
//! If a score with that name is attached directly to the trace, it IS the answer. A derived
//! value never overrides one a human or an evaluator stated about the trace itself.
//!
//! ## 3. A derived value is labelled as derived, and carries its `n`
//!
//! A rolled-up number is an inference, not a measurement, and is returned as
//! [`RolledScore`] with [`RolledScore::measured`] false, the function that produced it, and
//! how many spans contributed. This project ships Welch's-t gating and judge calibration
//! precisely because it does not want to overstate numbers; a derived score that rendered
//! identically to a measured one would undo that.
//!
//! ## 4. Absent stays absent
//!
//! If nothing in the trace carries the name, the answer is *no score* — never `0`, never a
//! fabricated pass. This matches how the rest of evald behaves: a Tier-1 evaluator SKIPs a
//! missing field rather than inventing a verdict, and `/metrics` omits a gauge it could not
//! sample rather than publishing a zero that reads as a real reading.
//!
//! ## The subtlety: a scored span is authoritative for its subtree
//!
//! Consider a RAG agent: `root → {retrieve, synthesize → {llm_call_1, llm_call_2}}`. If
//! `synthesize` carries a `faithfulness` score AND its two children do, averaging all three
//! double-counts the same work — the score on `synthesize` is *about* what its children did.
//!
//! So the walk takes the **shallowest** carrier of each name and does not descend past it.
//! `synthesize`'s score represents its subtree; the children's scores are the evidence
//! behind it, not additional samples beside it. Per name, so a trace can roll `faithfulness`
//! from one depth and `toxicity` from another.

use crate::model::{DataType, NormalizedSpan, Score, ScoreSource, ScoreTarget};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

/// How several span scores combine into one trace-level value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RollupFn {
    /// Arithmetic mean. The default for numeric scores: it is what someone asking "the
    /// trace's score" generally means, and it degrades gracefully as a trace grows.
    #[default]
    Mean,
    /// Worst case. **Usually the right choice for a CI gate** — one hallucinating step in a
    /// ten-step agent should fail the trace, and a mean would dilute it to near-passing.
    Min,
    /// Best case. Rarely what you want for quality; useful for "did any step succeed".
    Max,
    /// Total. The right function for additive quantities (cost, tokens, latency), where a
    /// mean would answer a question nobody asked.
    Sum,
    /// 1.0 only if every contributor is truthy (non-zero). The default for boolean scores:
    /// "did the trace pass" is the question, not "what fraction of steps passed".
    All,
    /// 1.0 if any contributor is truthy.
    Any,
}

impl RollupFn {
    pub fn as_str(self) -> &'static str {
        match self {
            RollupFn::Mean => "mean",
            RollupFn::Min => "min",
            RollupFn::Max => "max",
            RollupFn::Sum => "sum",
            RollupFn::All => "all",
            RollupFn::Any => "any",
        }
    }

    pub fn parse(s: &str) -> Option<RollupFn> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mean" | "avg" | "average" => Some(RollupFn::Mean),
            "min" => Some(RollupFn::Min),
            "max" => Some(RollupFn::Max),
            "sum" | "total" => Some(RollupFn::Sum),
            "all" | "and" => Some(RollupFn::All),
            "any" | "or" => Some(RollupFn::Any),
            _ => None,
        }
    }

    /// Combine contributor values. `values` is never empty (the caller only calls this when
    /// it has at least one contributor).
    fn apply(self, values: &[f64]) -> f64 {
        debug_assert!(!values.is_empty());
        match self {
            RollupFn::Mean => values.iter().sum::<f64>() / values.len() as f64,
            RollupFn::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
            RollupFn::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            RollupFn::Sum => values.iter().sum(),
            RollupFn::All => f64::from(values.iter().all(|v| *v != 0.0)),
            RollupFn::Any => f64::from(values.iter().any(|v| *v != 0.0)),
        }
    }
}

impl fmt::Display for RollupFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which function to use for which score name.
///
/// Per name, never inferred from the values: the right summary differs by metric, and a
/// single global function would be wrong for most of them. `cost` wants `sum`, a pass/fail
/// wants `all`, a quality gate usually wants `min`. Guessing from the data would make the
/// meaning of a number depend on which spans happened to be in the trace.
#[derive(Debug, Clone, Default)]
pub struct RollupConfig {
    by_name: BTreeMap<String, RollupFn>,
    /// Used for numeric scores with no explicit rule.
    default_numeric: RollupFn,
}

impl RollupConfig {
    /// Parse `name=fn` rules, e.g. `["faithfulness=min", "cost_usd=sum"]`.
    pub fn parse(rules: &[String]) -> Result<RollupConfig, String> {
        let mut by_name = BTreeMap::new();
        for spec in rules.iter().flat_map(|r| r.split(',')) {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            let (name, func) = spec
                .split_once('=')
                .ok_or_else(|| format!("malformed rollup rule {spec:?} (expected name=fn)"))?;
            let func = RollupFn::parse(func).ok_or_else(|| {
                format!(
                    "unknown rollup function {func:?} in {spec:?} \
                     (use mean | min | max | sum | all | any)"
                )
            })?;
            by_name.insert(name.trim().to_string(), func);
        }
        Ok(RollupConfig {
            by_name,
            default_numeric: RollupFn::Mean,
        })
    }

    /// The function for `name`, given the score's declared type.
    ///
    /// An explicit rule always wins. Otherwise boolean scores roll up with `all` — "did the
    /// trace pass" rather than "what fraction of steps passed" — and everything else with
    /// the numeric default.
    pub fn function_for(&self, name: &str, data_type: DataType) -> RollupFn {
        if let Some(f) = self.by_name.get(name) {
            return *f;
        }
        match data_type {
            DataType::Boolean => RollupFn::All,
            _ => self.default_numeric,
        }
    }
}

/// One trace-level score, measured or derived.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RolledScore {
    pub name: String,
    pub value: f64,
    /// `true` when a score with this name was attached to the trace itself. A measured score
    /// is returned verbatim and carries no rollup function.
    pub measured: bool,
    /// The function used, absent for a measured score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    /// How many spans contributed. `1` for a measured score.
    pub n: usize,
    /// The spans whose scores were combined, so a derived number is traceable back to its
    /// evidence rather than being an unexplained figure on a dashboard.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contributing_span_ids: Vec<String>,
    /// The type the rolled value carries.
    ///
    /// Boolean survives the rollup: `all` over pass/fail yields exactly 1 or 0, and calling
    /// that `Numeric` would turn a verdict into a measurement — a reader (and
    /// [`as_scores`], which writes these back) could no longer tell "every step passed"
    /// from "the mean was 1.0". Mixed inputs degrade to `Numeric`: a set that is not
    /// uniformly boolean has no boolean answer.
    pub data_type: DataType,
}

/// Compute every trace-level score for one trace.
///
/// `spans` are the trace's spans (any order) and `scores` every score attached to the trace
/// or to any of its spans. Returns one entry per distinct score name, sorted by name.
/// Names nothing carries are simply absent from the result.
///
/// Categorical and free-text scores are skipped: there is no defensible way to average
/// "helpful" and "terse", and inventing one would be exactly the kind of fabricated number
/// this module exists to avoid. They remain readable per-span.
pub fn rollup_trace(
    trace_id: &str,
    spans: &[NormalizedSpan],
    scores: &[Score],
    config: &RollupConfig,
) -> Vec<RolledScore> {
    // Scores attached to the trace itself are measured and final.
    let mut out: BTreeMap<String, RolledScore> = BTreeMap::new();
    for score in scores {
        if matches!(&score.target, ScoreTarget::Trace(id) if id == trace_id) {
            if let Some(value) = score.num_value {
                out.insert(
                    score.name.clone(),
                    RolledScore {
                        name: score.name.clone(),
                        value,
                        measured: true,
                        function: None,
                        n: 1,
                        contributing_span_ids: Vec::new(),
                        data_type: score.data_type,
                    },
                );
            }
        }
    }

    // Index the span scores by span id, keeping only numerically meaningful ones.
    let mut by_span: HashMap<&str, Vec<&Score>> = HashMap::new();
    for score in scores {
        if let ScoreTarget::Span(span_id) = &score.target {
            if score.num_value.is_some()
                && !matches!(score.data_type, DataType::Categorical | DataType::Text)
            {
                by_span.entry(span_id.as_str()).or_default().push(score);
            }
        }
    }
    if by_span.is_empty() {
        return out.into_values().collect();
    }

    let tree = SpanTree::build(spans);
    let names: BTreeSet<&str> = by_span
        .values()
        .flatten()
        .map(|s| s.name.as_str())
        .collect();

    for name in names {
        // A measured trace score wins; do not spend the walk.
        if out.contains_key(name) {
            continue;
        }
        let carriers = tree.shallowest_carriers(name, &by_span);
        if carriers.is_empty() {
            continue;
        }
        // The carriers' common type, or `Numeric` when they disagree. Taking the last
        // carrier's type instead would make the rollup FUNCTION depend on walk order — a
        // boolean seen last selects `all`, a numeric one selects `mean` — over the same
        // trace.
        let (mut values, mut span_ids) = (Vec::new(), Vec::new());
        let mut data_type: Option<DataType> = None;
        for (span_id, score) in carriers {
            if let Some(v) = score.num_value {
                values.push(v);
                span_ids.push(span_id.to_string());
                data_type = Some(match data_type {
                    Some(seen) if seen != score.data_type => DataType::Numeric,
                    Some(seen) => seen,
                    None => score.data_type,
                });
            }
        }
        let data_type = data_type.unwrap_or(DataType::Numeric);
        if values.is_empty() {
            continue;
        }
        let function = config.function_for(name, data_type);
        out.insert(
            name.to_string(),
            RolledScore {
                name: name.to_string(),
                value: function.apply(&values),
                measured: false,
                function: Some(function.as_str().to_string()),
                n: values.len(),
                contributing_span_ids: span_ids,
                data_type,
            },
        );
    }

    out.into_values().collect()
}

/// Persist derived trace scores back into the store as ordinary [`Score`]s.
///
/// Deliberately NOT done automatically on read. A derived value is a function of whatever
/// scores exist at the moment it is asked for; writing it down freezes an answer that a
/// later span score would otherwise have changed, and creates a second copy that can
/// disagree with the spans it came from. Callers that genuinely want a snapshot (an eval run
/// recording what it saw) opt in.
pub fn as_scores(trace_id: &str, rolled: &[RolledScore], ts_unix_nano: u64) -> Vec<Score> {
    rolled
        .iter()
        .filter(|r| !r.measured)
        .map(|r| Score {
            id: format!("rollup:{trace_id}:{}", r.name),
            target: ScoreTarget::Trace(trace_id.to_string()),
            name: r.name.clone(),
            num_value: Some(r.value),
            str_value: None,
            // The rolled type, not a blanket `Numeric`: `all` over a boolean score answers
            // "did every step pass", and persisting that as a measurement would lose the
            // distinction for every reader of the store afterwards.
            data_type: r.data_type,
            source: ScoreSource::Eval,
            // The provenance travels with the value: a reader who finds this in the store
            // months later can see it was computed, from how many spans, and how.
            comment: Some(format!(
                "derived by rollup ({} over {} span scores)",
                r.function.as_deref().unwrap_or("?"),
                r.n
            )),
            config_id: None,
            agg_stats: None,
            ts_unix_nano,
        })
        .collect()
}

/// Parent → children over one trace's spans.
struct SpanTree<'a> {
    children: HashMap<&'a str, Vec<&'a str>>,
    /// Spans with no parent inside this trace. A trace can legitimately have several — a
    /// dropped or not-yet-arrived parent leaves its children orphaned, and refusing to roll
    /// those up would lose real scores.
    roots: Vec<&'a str>,
}

impl<'a> SpanTree<'a> {
    fn build(spans: &'a [NormalizedSpan]) -> SpanTree<'a> {
        let present: BTreeSet<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut roots = Vec::new();
        for span in spans {
            match span.parent_span_id.as_deref() {
                // A parent outside this trace is treated as absent: the span is a root here.
                Some(parent) if present.contains(parent) && parent != span.span_id => {
                    children.entry(parent).or_default().push(&span.span_id);
                }
                _ => roots.push(span.span_id.as_str()),
            }
        }

        // Every span found so far has a parent chain that terminates outside the trace. A
        // chain that instead closes on ITSELF — a → b → a, which malformed instrumentation
        // and a replayed span id both produce — leaves every span in that cycle with a
        // present parent and therefore no root, so the walk in `shallowest_carriers` never
        // reaches it and each score it carries silently vanishes from the trace's rollup.
        // Rule 4 says absent stays absent; it does not say a score that IS there may
        // disappear. Give each unreachable component a deterministic entry point instead.
        let mut reachable: BTreeSet<&str> = BTreeSet::new();
        let walk = |from: &[&'a str], reachable: &mut BTreeSet<&'a str>| {
            let mut queue: std::collections::VecDeque<&'a str> = from.iter().copied().collect();
            while let Some(id) = queue.pop_front() {
                if !reachable.insert(id) {
                    continue;
                }
                if let Some(kids) = children.get(id) {
                    queue.extend(kids.iter().copied());
                }
            }
        };
        walk(&roots, &mut reachable);
        // `present` is ordered, so which span of a cycle becomes its root is stable across
        // runs rather than a hash-iteration coin flip — the same trace must roll up to the
        // same number every time it is asked for.
        let unreachable: Vec<&'a str> = present
            .iter()
            .copied()
            .filter(|id| !reachable.contains(id))
            .collect();
        for id in unreachable {
            if reachable.contains(id) {
                continue;
            }
            roots.push(id);
            walk(&[id], &mut reachable);
        }

        SpanTree { children, roots }
    }

    /// The shallowest spans carrying `name`, one per branch — descent into a subtree stops
    /// at the first span that carries it. See the module docs for why.
    fn shallowest_carriers<'s>(
        &self,
        name: &str,
        by_span: &'s HashMap<&'a str, Vec<&'a Score>>,
    ) -> Vec<(&'a str, &'s &'a Score)> {
        let mut found = Vec::new();
        // Breadth-first so "shallowest" is by real depth rather than by walk order, and with
        // a visited set so a malformed parent cycle cannot hang ingest-adjacent code.
        let mut queue: std::collections::VecDeque<&str> = self.roots.iter().copied().collect();
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        while let Some(span_id) = queue.pop_front() {
            if !visited.insert(span_id) {
                continue;
            }
            let carried = by_span
                .get(span_id)
                .and_then(|scores| scores.iter().find(|s| s.name == name));
            if let Some(score) = carried {
                // Authoritative for this subtree: record it and do not descend for `name`.
                if let Some((id, _)) = by_span.get_key_value(span_id).map(|(k, _)| (*k, ())) {
                    found.push((id, score));
                }
                continue;
            }
            if let Some(kids) = self.children.get(span_id) {
                queue.extend(kids.iter().copied());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;

    fn span(id: &str, parent: Option<&str>) -> NormalizedSpan {
        let mut s = test_span("t1", id, 1_700_000_000_000_000_000);
        s.parent_span_id = parent.map(str::to_string);
        s
    }

    fn span_score(span_id: &str, name: &str, value: f64) -> Score {
        Score {
            id: format!("{span_id}-{name}"),
            target: ScoreTarget::Span(span_id.to_string()),
            name: name.to_string(),
            num_value: Some(value),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: 1,
        }
    }

    fn cfg(rules: &[&str]) -> RollupConfig {
        RollupConfig::parse(&rules.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn absent_stays_absent_never_zero() {
        // The rule that matters most: a trace nothing scored has NO score, not a 0 that
        // would read as a total failure on a dashboard or fail a CI gate.
        let spans = [span("a", None), span("b", Some("a"))];
        assert!(rollup_trace("t1", &spans, &[], &cfg(&[])).is_empty());
    }

    #[test]
    fn a_measured_trace_score_is_returned_verbatim_and_never_overridden() {
        let spans = [span("a", None), span("b", Some("a"))];
        let mut trace_score = span_score("ignored", "faithfulness", 0.9);
        trace_score.target = ScoreTarget::Trace("t1".into());
        let scores = [
            trace_score,
            span_score("a", "faithfulness", 0.1),
            span_score("b", "faithfulness", 0.2),
        ];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled.len(), 1);
        assert_eq!(rolled[0].value, 0.9, "a stated trace score wins over spans");
        assert!(rolled[0].measured);
        assert_eq!(rolled[0].function, None);
    }

    #[test]
    fn sibling_span_scores_roll_up_and_are_marked_derived() {
        let spans = [span("a", None), span("b", Some("a")), span("c", Some("a"))];
        let scores = [
            span_score("b", "faithfulness", 0.4),
            span_score("c", "faithfulness", 0.6),
        ];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled.len(), 1);
        let r = &rolled[0];
        assert!((r.value - 0.5).abs() < 1e-9, "mean of 0.4 and 0.6");
        assert!(!r.measured, "a computed value must never look measured");
        assert_eq!(r.function.as_deref(), Some("mean"));
        assert_eq!(r.n, 2);
        assert_eq!(r.contributing_span_ids, vec!["b", "c"]);
    }

    /// The subtle rule: a scored parent represents its subtree, so its children's scores are
    /// the evidence behind it, not extra samples beside it.
    #[test]
    fn a_scored_span_is_authoritative_for_its_subtree() {
        //        root
        //      /      \
        //  retrieve   synthesize (scored 0.2)
        //                 |    \
        //              llm1(1.0) llm2(1.0)
        let spans = [
            span("root", None),
            span("retrieve", Some("root")),
            span("synthesize", Some("root")),
            span("llm1", Some("synthesize")),
            span("llm2", Some("synthesize")),
        ];
        let scores = [
            span_score("synthesize", "faithfulness", 0.2),
            span_score("llm1", "faithfulness", 1.0),
            span_score("llm2", "faithfulness", 1.0),
        ];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled.len(), 1);
        // Averaging all three would give 0.73 and hide the bad synthesis step behind the
        // two sub-calls it summarises.
        assert!((rolled[0].value - 0.2).abs() < 1e-9, "{:?}", rolled[0]);
        assert_eq!(rolled[0].n, 1);
        assert_eq!(rolled[0].contributing_span_ids, vec!["synthesize"]);
    }

    #[test]
    fn the_function_is_per_name_and_declared() {
        let spans = [span("a", None), span("b", Some("a")), span("c", Some("a"))];
        let scores = [
            span_score("b", "faithfulness", 0.2),
            span_score("c", "faithfulness", 1.0),
            span_score("b", "cost_usd", 1.5),
            span_score("c", "cost_usd", 2.5),
        ];
        // A quality gate wants the worst step; cost wants the total. One global function
        // could not be right for both.
        let rolled = rollup_trace(
            "t1",
            &spans,
            &scores,
            &cfg(&["faithfulness=min", "cost_usd=sum"]),
        );
        let by_name: BTreeMap<_, _> = rolled.iter().map(|r| (r.name.as_str(), r)).collect();
        assert!((by_name["faithfulness"].value - 0.2).abs() < 1e-9);
        assert_eq!(by_name["faithfulness"].function.as_deref(), Some("min"));
        assert!((by_name["cost_usd"].value - 4.0).abs() < 1e-9);
        assert_eq!(by_name["cost_usd"].function.as_deref(), Some("sum"));
    }

    #[test]
    fn boolean_scores_default_to_all_not_mean() {
        let spans = [span("a", None), span("b", Some("a")), span("c", Some("a"))];
        let mut pass = span_score("b", "passed", 1.0);
        pass.data_type = DataType::Boolean;
        let mut fail = span_score("c", "passed", 0.0);
        fail.data_type = DataType::Boolean;
        let rolled = rollup_trace("t1", &spans, &[pass, fail], &cfg(&[]));
        // A mean would say 0.5 — "half passed". The question is "did the trace pass".
        assert_eq!(rolled[0].value, 0.0);
        assert_eq!(rolled[0].function.as_deref(), Some("all"));
        assert_eq!(rolled[0].data_type, DataType::Boolean);

        // Selecting `all` must not depend on which carrier the walk happened to see last,
        // so a set that is NOT uniformly boolean degrades to the numeric default rather
        // than answering a boolean question about numbers.
        let mut mixed_bool = span_score("b", "mixed", 1.0);
        mixed_bool.data_type = DataType::Boolean;
        let mixed_num = span_score("c", "mixed", 0.4);
        let rolled = rollup_trace("t1", &spans, &[mixed_bool, mixed_num], &cfg(&[]));
        assert_eq!(rolled[0].data_type, DataType::Numeric);
        assert_eq!(rolled[0].function.as_deref(), Some("mean"));
    }

    #[test]
    fn categorical_and_text_scores_are_skipped_not_averaged() {
        let spans = [span("a", None), span("b", Some("a"))];
        let mut label = span_score("b", "tone", 1.0);
        label.data_type = DataType::Categorical;
        label.str_value = Some("terse".into());
        assert!(
            rollup_trace("t1", &spans, &[label], &cfg(&[])).is_empty(),
            "there is no defensible mean of two category labels"
        );
    }

    #[test]
    fn orphaned_spans_still_contribute() {
        // A parent that never arrived (dropped, or still in flight) must not silently
        // discard its children's scores — they are real measurements.
        let spans = [
            span("b", Some("missing-parent")),
            span("c", Some("missing-parent")),
        ];
        let scores = [span_score("b", "q", 1.0), span_score("c", "q", 0.0)];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled[0].n, 2);
        assert!((rolled[0].value - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_parent_cycle_terminates_and_keeps_its_scores() {
        // Malformed parent pointers are external input; the walk must not hang.
        //
        // Regression: it did not hang, it silently lost everything. Both spans in a cycle
        // have a present parent, so neither was a root, and a walk that starts at the roots
        // visits neither — `q` came back as no score at all, which rule 4 reserves for a
        // name nothing carries. An absent score and a dropped score read identically on a
        // dashboard and mean opposite things.
        let spans = [span("a", Some("b")), span("b", Some("a"))];
        let scores = [span_score("a", "q", 1.0), span_score("b", "q", 0.0)];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled.len(), 1, "the cycle's scores must not vanish");
        assert_eq!(rolled[0].n, 1, "a cycle still has ONE shallowest carrier");
        // Deterministic: `a` sorts first, so `a` is the component's entry point on every
        // run. Which member is picked is arbitrary; that it is the SAME one every time is
        // not — an answer that changed between two identical requests would be worse than
        // either value.
        assert_eq!(rolled[0].contributing_span_ids, ["a"]);
        assert_eq!(rolled[0].value, 1.0);

        // A cycle hanging off a real root keeps the ordinary walk: `r` is reachable, and
        // the detached `x ↔ y` pair gets its own entry point rather than being dropped.
        let spans = [span("r", None), span("x", Some("y")), span("y", Some("x"))];
        let scores = [span_score("r", "q", 1.0), span_score("y", "q", 0.0)];
        let rolled = rollup_trace("t1", &spans, &scores, &cfg(&[]));
        assert_eq!(rolled[0].n, 2);
        assert!((rolled[0].value - 0.5).abs() < 1e-9);
    }

    #[test]
    fn config_rejects_malformed_rules_at_parse_time() {
        assert!(RollupConfig::parse(&["faithfulness".into()]).is_err());
        assert!(RollupConfig::parse(&["faithfulness=median".into()]).is_err());
        assert!(RollupConfig::parse(&["ok=min".into()]).is_ok());
    }

    #[test]
    fn derived_scores_carry_their_provenance_when_persisted() {
        let rolled = vec![
            RolledScore {
                name: "faithfulness".into(),
                value: 0.5,
                measured: false,
                function: Some("mean".into()),
                n: 2,
                contributing_span_ids: vec!["b".into(), "c".into()],
                data_type: DataType::Numeric,
            },
            RolledScore {
                name: "measured_one".into(),
                value: 1.0,
                measured: true,
                function: None,
                n: 1,
                contributing_span_ids: vec![],
                data_type: DataType::Numeric,
            },
            RolledScore {
                name: "passed".into(),
                value: 0.0,
                measured: false,
                function: Some("all".into()),
                n: 3,
                contributing_span_ids: vec!["b".into()],
                data_type: DataType::Boolean,
            },
        ];
        let persisted = as_scores("t1", &rolled, 42);
        // Only the derived one is written — re-persisting a measured score would duplicate
        // what is already there.
        // Only the derived ones are written — re-persisting a measured score would
        // duplicate what is already there.
        assert_eq!(persisted.len(), 2);
        assert_eq!(persisted[0].name, "faithfulness");
        assert!(persisted[0]
            .comment
            .as_deref()
            .unwrap()
            .contains("mean over 2"));
        assert!(matches!(&persisted[0].target, ScoreTarget::Trace(t) if t == "t1"));
        // A boolean rollup stays boolean once written down. Flattening it to `Numeric`
        // would leave the store unable to say whether 0 meant "a step failed" or "the mean
        // came out at zero".
        assert_eq!(persisted[1].name, "passed");
        assert_eq!(persisted[1].data_type, DataType::Boolean);
        assert_eq!(persisted[0].data_type, DataType::Numeric);
    }
}
