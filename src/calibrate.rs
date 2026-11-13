//! Judge calibration + bias correction (`evald eval calibrate`).
//!
//! An LLM-as-judge ([`crate::judge`]) is only as trustworthy as its agreement with a human.
//! This command measures that agreement **offline, against your own gold labels**: it pairs
//! each judge score with the human annotation on the SAME span/trace (evald is OTel-native, so
//! the span/trace id IS the join key — no extra bookkeeping) and reports how far the judge
//! drifts from the human, whether that drift is a systematic bias or per-item noise, and — for
//! bias correction — the affine map `human ≈ intercept + slope · judge` that best aligns the
//! judge's scale to the human's.
//!
//! Calibration runs entirely on your own data: it pairs each judge score with the human label
//! on the same span in your local store — no network, no external service, no configuration.
//!
//! Fully offline and deterministic: it reads persisted [`Score`]s, makes no network call, and
//! reuses the dependency-free Student's-t machinery in [`crate::stats`] for the bias confidence
//! interval. With `--fail-on-divergence` it doubles as a CI gate (exit non-zero when the judge
//! has drifted far enough from the human ground truth to need recalibration).
//!
//! Two more figures, because a score is only as good as its agreement and its repeatability:
//! - **Pass/fail agreement** with the human, chance-corrected ([`Agreement`], Cohen's kappa):
//!   a judge that says "pass" to everything agrees with a mostly-passing human set most of the
//!   time and is still useless, and kappa says so.
//! - **Repeat stability** ([`Stability`]): the same judge on the same items over N runs (made
//!   with `eval run --no-cache`), matched by item id. How often the verdict held, how far the
//!   scores moved, and Fleiss' kappa over the verdicts.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{ScoreSource, ScoreTarget, Store, StoreConfig};

/// The calibration of one judge against the paired human annotations.
///
/// All quantities are over the `n` targets that carried BOTH a judge score and a human score;
/// `bias`, `mae`, `rmse` are in the score's own units (rails emit `[0, 1]`). `bias` is signed
/// `judge − human`, so a positive bias means the judge is **over-scoring** relative to the
/// human. The affine `(intercept, slope)` regresses human on judge, so applying
/// `intercept + slope · judge` to a future judge score is the bias-corrected estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// Number of paired (judge, human) observations the calibration is computed over.
    pub n: usize,
    /// Mean signed error `mean(judge − human)`. Positive ⇒ the judge scores higher than the
    /// human on average (systematic over-scoring); negative ⇒ under-scoring.
    pub bias: f64,
    /// Mean absolute error `mean(|judge − human|)` — per-item disagreement, blind to sign.
    pub mae: f64,
    /// Root-mean-square error `sqrt(mean((judge − human)²))` — like MAE but penalizes large
    /// single-item disagreements more.
    pub rmse: f64,
    /// Pearson correlation between judge and human scores, clamped to `[-1, 1]`. `0.0` when
    /// either side has zero variance (no spread to correlate). High correlation with high bias
    /// means the judge ranks correctly but on a shifted scale — exactly what the affine
    /// correction fixes.
    pub slope_pearson_note: PearsonNote,
    /// Pearson correlation `r` (see [`Calibration::slope_pearson_note`]).
    pub pearson: f64,
    /// Slope of the bias-correcting affine map `human ≈ intercept + slope · judge` (the
    /// least-squares regression of human on judge). `0.0` when the judge has zero variance.
    pub slope: f64,
    /// Intercept of the bias-correcting affine map (see [`Calibration::slope`]).
    pub intercept: f64,
    /// `(1 − alpha)` confidence interval on `bias` from a paired-difference Student's-t test.
    pub bias_ci: (f64, f64),
    /// Whether the bias CI excludes `0` — i.e. the judge has a **statistically significant**
    /// systematic offset (not explainable by sampling noise at this `alpha`).
    pub bias_significant: bool,
    /// The verdict: `true` ⇒ the judge has drifted far enough from the human to warrant
    /// recalibration — either per-item disagreement (`mae`) exceeds the divergence threshold,
    /// or the bias is both significant and larger than the threshold.
    pub recalibrate: bool,
}

/// A marker carried in [`Calibration`] purely so the printer can footnote a degenerate Pearson
/// (zero variance on one side) instead of silently showing `0.000` as if it were a real
/// measured non-correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PearsonNote {
    /// Both sides had spread — `pearson` is a real correlation.
    Defined,
    /// Judge and/or human scores were all identical — correlation is undefined, reported `0.0`.
    Degenerate,
}

/// Compute the calibration of a judge from its paired (judge, human) scores.
///
/// Returns `None` when there are fewer than 2 pairs — variance, the regression, and the
/// confidence interval are all undefined for a single point, and a one-pair "calibration"
/// would be dangerously misleading.
///
/// `alpha` is the significance level for the bias confidence interval (e.g. `0.05` → 95% CI).
/// `divergence_threshold` is how far (in score units) the judge may drift before
/// [`Calibration::recalibrate`] fires.
pub fn calibrate(
    pairs: &[(f64, f64)],
    alpha: f64,
    divergence_threshold: f64,
) -> Option<Calibration> {
    let n = pairs.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mean_j = pairs.iter().map(|p| p.0).sum::<f64>() / nf;
    let mean_h = pairs.iter().map(|p| p.1).sum::<f64>() / nf;
    let bias = mean_j - mean_h;

    let mut cov = 0.0; // Σ (j−j̄)(h−h̄)
    let mut ss_j = 0.0; // Σ (j−j̄)²
    let mut ss_h = 0.0; // Σ (h−h̄)²
    let mut sum_abs = 0.0; // Σ |j−h|
    let mut sum_sq = 0.0; // Σ (j−h)²
    let mut ss_d = 0.0; // Σ (d−d̄)² for the paired difference d = j−h, d̄ = bias
    for &(j, h) in pairs {
        let dj = j - mean_j;
        let dh = h - mean_h;
        cov += dj * dh;
        ss_j += dj * dj;
        ss_h += dh * dh;
        let d = j - h;
        sum_abs += d.abs();
        sum_sq += d * d;
        let dd = d - bias;
        ss_d += dd * dd;
    }

    let mae = sum_abs / nf;
    let rmse = (sum_sq / nf).sqrt();

    // Pearson and the regression slope share `cov`; both are undefined if the judge (or, for
    // Pearson, either side) has no spread. Report `0.0` and flag it rather than emit a NaN.
    let (pearson, slope_pearson_note) = if ss_j > 0.0 && ss_h > 0.0 {
        (
            (cov / (ss_j.sqrt() * ss_h.sqrt())).clamp(-1.0, 1.0),
            PearsonNote::Defined,
        )
    } else {
        (0.0, PearsonNote::Degenerate)
    };
    let slope = if ss_j > 0.0 { cov / ss_j } else { 0.0 };
    let intercept = mean_h - slope * mean_j;

    // Paired-difference t-interval on the bias: d̄ ± t_{1−α/2, n−1} · sqrt(var_d / n).
    let var_d = ss_d / (nf - 1.0); // unbiased (ddof = 1)
    let se = (var_d / nf).sqrt();
    let tcrit = crate::stats::student_t_quantile(1.0 - alpha / 2.0, nf - 1.0);
    let bias_ci = (bias - tcrit * se, bias + tcrit * se);
    let bias_significant = bias_ci.0 > 0.0 || bias_ci.1 < 0.0;

    // Recalibrate when the judge disagrees item-by-item beyond the threshold (mae), OR carries a
    // systematic offset that is both real (significant) and large (beyond the threshold). A tiny
    // but significant bias on a huge sample is not worth recalibrating; a large one is.
    let recalibrate =
        mae > divergence_threshold || (bias.abs() > divergence_threshold && bias_significant);

    Some(Calibration {
        n,
        bias,
        mae,
        rmse,
        slope_pearson_note,
        pearson,
        slope,
        intercept,
        bias_ci,
        bias_significant,
        recalibrate,
    })
}

/// Pass/fail agreement between the judge and the human at one cut (a score ≥ `cut` passes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agreement {
    pub cut: f64,
    pub n: usize,
    /// Share of pairs where both said pass, or both said fail.
    pub observed: f64,
    /// Cohen's kappa over the verdicts; `None` when it is undefined (one verdict throughout).
    pub kappa: Option<f64>,
}

/// [`Agreement`] over paired `(judge, human)` scores. `None` with no pairs.
pub fn pass_fail_agreement(pairs: &[(f64, f64)], cut: f64) -> Option<Agreement> {
    if pairs.is_empty() {
        return None;
    }
    let verdicts: Vec<(bool, bool)> = pairs.iter().map(|&(j, h)| (j >= cut, h >= cut)).collect();
    let agree = verdicts.iter().filter(|(j, h)| j == h).count();
    Some(Agreement {
        cut,
        n: pairs.len(),
        observed: agree as f64 / pairs.len() as f64,
        kappa: crate::stats::cohen_kappa(&verdicts),
    })
}

/// How one judge's scores held up over repeat runs on the same items.
#[derive(Debug, Clone, PartialEq)]
pub struct Stability {
    pub runs: usize,
    /// Items scored in every run (the ones measured).
    pub items: usize,
    /// Items some run did not score, left out.
    pub left_out: usize,
    pub cut: f64,
    /// Share of items whose verdict (score ≥ `cut`) was the same in every run.
    pub verdict_agreement: f64,
    /// Mean over items of the standard deviation of that item's scores across runs.
    pub mean_sd: f64,
    /// The largest spread (max − min) any item's scores showed.
    pub max_range: f64,
    /// Fleiss' kappa over the verdicts, the runs as raters; `None` when undefined.
    pub fleiss_kappa: Option<f64>,
    /// Up to five items with the widest spread, with their scores in run order.
    pub least_stable: Vec<(String, Vec<f64>)>,
}

/// [`Stability`] over N runs' per-item values of one judge. An error names a run whose items
/// cannot be matched (no ids, repeated ids); `Ok(None)` when fewer than 2 runs or no item was
/// scored in all of them.
pub fn stability(
    runs: &[(String, crate::eval::ItemValues)],
    cut: f64,
) -> anyhow::Result<Option<Stability>> {
    if runs.len() < 2 {
        return Ok(None);
    }
    let maps = runs
        .iter()
        .map(|(id, v)| v.by_key().map_err(|why| anyhow::anyhow!("run {id}: {why}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let all: std::collections::BTreeSet<&str> =
        maps.iter().flat_map(|m| m.keys().copied()).collect();
    let mut rows: Vec<(String, Vec<f64>)> = all
        .iter()
        .filter_map(|k| {
            let vals: Option<Vec<f64>> = maps.iter().map(|m| m.get(k).copied()).collect();
            vals.map(|v| (k.to_string(), v))
        })
        .collect();
    if rows.is_empty() {
        return Ok(None);
    }
    let (n, r) = (rows.len(), runs.len() as f64);
    let mut same = 0usize;
    let mut sd_sum = 0.0;
    let mut max_range = 0.0_f64;
    let mut counts = Vec::with_capacity(n);
    for (_, vals) in &rows {
        let passes = vals.iter().filter(|&&v| v >= cut).count() as u64;
        if passes == 0 || passes == vals.len() as u64 {
            same += 1;
        }
        counts.push(vec![passes, vals.len() as u64 - passes]);
        let mean = vals.iter().sum::<f64>() / r;
        sd_sum += (vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (r - 1.0)).sqrt();
        let (lo, hi) = vals
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        max_range = max_range.max(hi - lo);
    }
    let spread = |v: &[f64]| {
        v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - v.iter().copied().fold(f64::INFINITY, f64::min)
    };
    rows.sort_by(|a, b| {
        spread(&b.1)
            .total_cmp(&spread(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    let least_stable = rows
        .into_iter()
        .filter(|(_, v)| spread(v) > 0.0)
        .take(5)
        .collect();
    Ok(Some(Stability {
        runs: runs.len(),
        items: n,
        left_out: all.len() - n,
        cut,
        verdict_agreement: same as f64 / n as f64,
        mean_sd: sd_sum / n as f64,
        max_range,
        fleiss_kappa: crate::stats::fleiss_kappa(&counts),
        least_stable,
    }))
}

/// Pair a judge's scores with human annotations on the same target, reading from the store.
///
/// For each span/trace target, takes the **newest** judge score (`source = Eval`, `name =
/// judge`) and the newest human score (`source = Human`; when `human_name` is given, only that
/// name) — both must carry a numeric value — and pairs their values. `Run`-targeted scores
/// (the per-evaluator aggregates) are skipped: calibration is a per-item comparison, and a
/// human never annotates a run aggregate.
pub fn collect_pairs(
    store: &Store,
    judge: &str,
    human_name: Option<&str>,
) -> std::io::Result<Vec<(f64, f64)>> {
    // `list_scores` is newest-first, so the first value we see per target IS the newest.
    let all = store.list_scores(usize::MAX)?;
    let mut judge_by_target: BTreeMap<String, f64> = BTreeMap::new();
    let mut human_by_target: BTreeMap<String, f64> = BTreeMap::new();
    for s in all {
        if matches!(s.target, ScoreTarget::Run(_)) {
            continue; // aggregates, not per-item — never paired
        }
        let Some(v) = s.num_value else { continue };
        let key = s.target.key();
        match s.source {
            ScoreSource::Eval if s.name == judge => {
                judge_by_target.entry(key).or_insert(v);
            }
            ScoreSource::Human if human_name.is_none_or(|h| h == s.name) => {
                human_by_target.entry(key).or_insert(v);
            }
            _ => {}
        }
    }
    // Inner join on the target key (BTreeMap ⇒ deterministic order).
    let pairs = judge_by_target
        .iter()
        .filter_map(|(key, &jv)| human_by_target.get(key).map(|&hv| (jv, hv)))
        .collect();
    Ok(pairs)
}

/// `evald eval calibrate` — measure a judge against the paired human labels in `data_dir`.
///
/// Returns whether the CI gate passes: always `true` unless `fail_on_divergence` is set and the
/// judge needs recalibration. Insufficient data (fewer than 2 pairs) is informational, not a
/// gate failure — there is nothing to have regressed.
/// `cut` is the pass/fail line for the agreement and stability figures; `runs`, when given (two
/// or more), are repeat runs of the judge to measure its stability over.
#[allow(clippy::too_many_arguments)]
pub fn calibrate_command(
    judge: &str,
    human: Option<&str>,
    data_dir: &Path,
    alpha: f64,
    threshold: f64,
    fail_on_divergence: bool,
    cut: f64,
    runs: &[String],
) -> anyhow::Result<bool> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;

    let pairs =
        collect_pairs(&store, judge, human).map_err(|e| anyhow::anyhow!("reading scores: {e}"))?;

    if !runs.is_empty() {
        anyhow::ensure!(
            runs.len() >= 2,
            "--runs takes two or more run ids (repeat runs of the same judge on the same items)"
        );
        let mut values = Vec::with_capacity(runs.len());
        for run in runs {
            let mut items = crate::eval::run_items(&store, run)?;
            let v = items.remove(judge).ok_or_else(|| {
                anyhow::anyhow!(
                    "run {run} has no per-item values for {judge:?}: it did not run that judge, \
                     or it was written before evald recorded them"
                )
            })?;
            values.push((run.clone(), v));
        }
        match stability(&values, cut)? {
            Some(st) => print_stability(judge, &st),
            None => println!(
                "Stability: no item was scored in all {} runs of {judge:?}.",
                runs.len()
            ),
        }
        if pairs.len() < 2 {
            // Stability was asked for; a missing human set is not news here.
            return Ok(true);
        }
        println!();
    }

    match calibrate(&pairs, alpha, threshold) {
        None => {
            println!(
                "calibrate: not enough paired scores for judge {judge:?} — need \u{2265}2 spans/traces \
                 carrying BOTH a `{judge}` eval score (source=eval) and a human annotation \
                 (source=human); found {}.",
                pairs.len()
            );
            println!(
                "  Annotate the same spans the judge scored (e.g. POST /v1/span_annotations), then re-run."
            );
            Ok(true)
        }
        Some(c) => {
            print_calibration(judge, human, &c, alpha, threshold);
            if let Some(a) = pass_fail_agreement(&pairs, cut) {
                print_agreement(&a);
            }
            let gate_fails = fail_on_divergence && c.recalibrate;
            Ok(!gate_fails)
        }
    }
}

fn kappa_cell(k: Option<f64>) -> String {
    match k {
        Some(k) => format!("{k:+.3} ({})", crate::stats::kappa_words(k)),
        None => "undefined (one verdict throughout)".to_string(),
    }
}

fn print_agreement(a: &Agreement) {
    println!(
        "  pass/fail agreement : {:.1}% of {} at score \u{2265} {:.2}; Cohen's \u{03ba} {}",
        a.observed * 100.0,
        a.n,
        a.cut,
        kappa_cell(a.kappa)
    );
}

fn print_stability(judge: &str, s: &Stability) {
    println!("Stability: judge `{judge}` over {} runs", s.runs);
    println!("  items in every run  : {}", s.items);
    if s.left_out > 0 {
        println!(
            "  left out            : {} (not scored in every run)",
            s.left_out
        );
    }
    println!(
        "  same verdict        : {:.1}% of items in every run (score \u{2265} {:.2})",
        s.verdict_agreement * 100.0,
        s.cut
    );
    println!(
        "  Fleiss' \u{03ba}           : {}",
        kappa_cell(s.fleiss_kappa)
    );
    println!("  mean per-item SD    : {:.4}", s.mean_sd);
    println!("  widest spread       : {:.4}", s.max_range);
    for (key, vals) in &s.least_stable {
        let vals: Vec<String> = vals.iter().map(|v| format!("{v:.2}")).collect();
        println!("    {key}: {}", vals.join(" "));
    }
}

/// Human-readable calibration report on stdout.
fn print_calibration(
    judge: &str,
    human: Option<&str>,
    c: &Calibration,
    alpha: f64,
    threshold: f64,
) {
    let human_label = human.unwrap_or("human (any)");
    println!("Calibration: judge `{judge}` vs {human_label}");
    println!("  paired observations : {}", c.n);
    let dir = if c.bias > 0.0 {
        "judge over-scores"
    } else if c.bias < 0.0 {
        "judge under-scores"
    } else {
        "no mean offset"
    };
    println!("  bias (judge − human): {:+.4}  ({dir})", c.bias);
    println!(
        "  {:.0}% CI on bias    : [{:+.4}, {:+.4}]  {}",
        (1.0 - alpha) * 100.0,
        c.bias_ci.0,
        c.bias_ci.1,
        if c.bias_significant {
            "(excludes 0 → significant)"
        } else {
            "(includes 0 → within noise)"
        }
    );
    println!("  MAE                 : {:.4}", c.mae);
    println!("  RMSE                : {:.4}", c.rmse);
    match c.slope_pearson_note {
        PearsonNote::Defined => println!("  Pearson r           : {:+.4}", c.pearson),
        PearsonNote::Degenerate => {
            println!("  Pearson r           : n/a (judge or human scores are all identical)")
        }
    }
    println!(
        "  bias correction     : human \u{2248} {:+.4} + {:.4}\u{00b7}judge",
        c.intercept, c.slope
    );
    println!(
        "                        (apply to a future judge score to align it to the human scale)"
    );
    println!("  divergence threshold: {threshold:.4}");
    if c.recalibrate {
        println!(
            "  VERDICT             : RECALIBRATE — judge has drifted from the human ground truth"
        );
    } else {
        println!("  VERDICT             : OK — judge agrees with the human within tolerance");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataType, Score, ScoreSource, ScoreTarget, Store, StoreConfig};

    fn calib(pairs: &[(f64, f64)]) -> Calibration {
        calibrate(pairs, 0.05, 0.2).expect("\u{2265}2 pairs")
    }

    #[test]
    fn fewer_than_two_pairs_is_none() {
        assert!(calibrate(&[], 0.05, 0.2).is_none());
        assert!(calibrate(&[(0.5, 0.5)], 0.05, 0.2).is_none());
    }

    #[test]
    fn perfect_agreement_is_zero_error_and_ok() {
        let c = calib(&[(0.2, 0.2), (0.8, 0.8), (0.5, 0.5)]);
        assert!(c.bias.abs() < 1e-12);
        assert!(c.mae < 1e-12);
        assert!(c.rmse < 1e-12);
        assert!((c.pearson - 1.0).abs() < 1e-9, "perfectly correlated");
        assert!((c.slope - 1.0).abs() < 1e-9);
        assert!(c.intercept.abs() < 1e-9);
        assert!(!c.bias_significant, "no offset to be significant");
        assert!(!c.recalibrate);
    }

    #[test]
    fn constant_positive_offset_is_detected_and_correctable() {
        // Judge is exactly +0.30 above the human on every item: a pure, large, systematic bias.
        let pairs = [(0.4, 0.1), (0.7, 0.4), (0.9, 0.6), (0.5, 0.2)];
        let c = calib(&pairs);
        assert!((c.bias - 0.30).abs() < 1e-9, "bias = +0.30, got {}", c.bias);
        assert!((c.mae - 0.30).abs() < 1e-9);
        // A constant offset is perfectly correlated and corrected by slope 1, intercept −bias.
        assert!((c.pearson - 1.0).abs() < 1e-9);
        assert!((c.slope - 1.0).abs() < 1e-9);
        assert!((c.intercept + 0.30).abs() < 1e-9, "intercept ≈ −0.30");
        // Zero scatter ⇒ a razor-thin CI that excludes 0 ⇒ significant ⇒ recalibrate.
        assert!(c.bias_significant);
        assert!(c.recalibrate, "0.30 > 0.20 threshold and significant");
    }

    #[test]
    fn small_offset_within_threshold_does_not_gate() {
        // A +0.05 mean offset: real-ish but under the 0.20 divergence threshold.
        let pairs = [(0.55, 0.5), (0.65, 0.6), (0.45, 0.4), (0.75, 0.7)];
        let c = calib(&pairs);
        assert!((c.bias - 0.05).abs() < 1e-9);
        assert!(c.mae < 0.2, "per-item disagreement under threshold");
        assert!(
            !c.recalibrate,
            "small bias under threshold must not recalibrate"
        );
    }

    #[test]
    fn noisy_disagreement_recalibrates_even_with_zero_mean_bias() {
        // Mean bias cancels to ~0, but item-by-item the judge swings wildly: MAE is large, so the
        // judge is untrustworthy and must recalibrate despite an unbiased mean.
        let pairs = [(0.9, 0.1), (0.1, 0.9), (0.8, 0.2), (0.2, 0.8)];
        let c = calib(&pairs);
        assert!(c.bias.abs() < 1e-9, "mean error cancels");
        assert!(c.mae > 0.2, "but per-item MAE is large: {}", c.mae);
        assert!(c.recalibrate, "high MAE alone must trigger recalibration");
    }

    #[test]
    fn degenerate_pearson_is_flagged_not_nan() {
        // Judge gives the same score to everything ⇒ no spread ⇒ Pearson undefined.
        let pairs = [(0.5, 0.2), (0.5, 0.8), (0.5, 0.5)];
        let c = calib(&pairs);
        assert_eq!(c.slope_pearson_note, PearsonNote::Degenerate);
        assert_eq!(c.pearson, 0.0);
        assert_eq!(c.slope, 0.0, "no judge spread ⇒ flat correction");
        assert!(c.pearson.is_finite() && c.slope.is_finite() && c.intercept.is_finite());
    }

    fn human_score(target: ScoreTarget, name: &str, v: f64, ts: u64) -> Score {
        Score {
            id: format!("h:{}:{ts}", target.key()),
            target,
            name: name.to_string(),
            num_value: Some(v),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Human,
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: ts,
        }
    }

    fn judge_score(target: ScoreTarget, name: &str, v: f64, ts: u64) -> Score {
        Score {
            id: format!("j:{}:{ts}", target.key()),
            target,
            name: name.to_string(),
            num_value: Some(v),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: ts,
        }
    }

    #[tokio::test]
    async fn collect_pairs_joins_judge_and_human_on_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..Default::default()
            },
        )
        .unwrap();

        store
            .put_scores(&[
                // span A: both present → paired (0.8, 0.6)
                judge_score(ScoreTarget::Span("a".into()), "judge_g_eval", 0.8, 1),
                human_score(ScoreTarget::Span("a".into()), "human_quality", 0.6, 2),
                // span B: both present → paired (0.4, 0.5)
                judge_score(ScoreTarget::Span("b".into()), "judge_g_eval", 0.4, 1),
                human_score(ScoreTarget::Span("b".into()), "human_quality", 0.5, 2),
                // span C: judge only → dropped
                judge_score(ScoreTarget::Span("c".into()), "judge_g_eval", 0.9, 1),
                // span D: human only → dropped
                human_score(ScoreTarget::Span("d".into()), "human_quality", 0.1, 2),
                // span E: a DIFFERENT judge name → not collected for judge_g_eval
                judge_score(ScoreTarget::Span("e".into()), "judge_toxicity", 0.3, 1),
                human_score(ScoreTarget::Span("e".into()), "human_quality", 0.3, 2),
                // run aggregate carrying judge_g_eval name → must be ignored (Run target)
                judge_score(ScoreTarget::Run("r1".into()), "judge_g_eval", 0.99, 1),
            ])
            .unwrap();

        let pairs = collect_pairs(&store, "judge_g_eval", None).unwrap();
        assert_eq!(pairs.len(), 2, "only spans A and B are joined: {pairs:?}");
        assert!(pairs.contains(&(0.8, 0.6)));
        assert!(pairs.contains(&(0.4, 0.5)));
    }

    #[tokio::test]
    async fn collect_pairs_takes_newest_score_per_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..Default::default()
            },
        )
        .unwrap();
        store
            .put_scores(&[
                judge_score(ScoreTarget::Span("a".into()), "j", 0.2, 1), // older
                judge_score(ScoreTarget::Span("a".into()), "j", 0.7, 9), // newer → wins
                human_score(ScoreTarget::Span("a".into()), "h", 0.5, 1),
                human_score(ScoreTarget::Span("a".into()), "h", 0.6, 9), // newer → wins
            ])
            .unwrap();
        let pairs = collect_pairs(&store, "j", None).unwrap();
        assert_eq!(pairs, vec![(0.7, 0.6)]);
    }

    #[tokio::test]
    async fn collect_pairs_filters_by_human_name_when_given() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..Default::default()
            },
        )
        .unwrap();
        store
            .put_scores(&[
                judge_score(ScoreTarget::Span("a".into()), "j", 0.8, 1),
                // newest human on A is the "wrong" reviewer; the named one is older but selected
                human_score(ScoreTarget::Span("a".into()), "alice", 0.7, 5),
                human_score(ScoreTarget::Span("a".into()), "bob", 0.1, 9),
            ])
            .unwrap();
        let only_alice = collect_pairs(&store, "j", Some("alice")).unwrap();
        assert_eq!(
            only_alice,
            vec![(0.8, 0.7)],
            "bob's newer score is excluded"
        );
        let any = collect_pairs(&store, "j", None).unwrap();
        assert_eq!(any, vec![(0.8, 0.1)], "unfiltered takes bob (newest)");
    }

    #[test]
    fn agreement_is_chance_corrected() {
        // 50 pairs laid out as the classic table: both pass 20, judge-only 5, human-only 10,
        // both fail 15. p_o = 0.7, κ = 0.4.
        let mut pairs = Vec::new();
        for (j, h, n) in [
            (0.9, 0.8, 20),
            (0.7, 0.2, 5),
            (0.1, 0.6, 10),
            (0.2, 0.0, 15),
        ] {
            pairs.extend(std::iter::repeat_n((j, h), n));
        }
        let a = pass_fail_agreement(&pairs, 0.5).unwrap();
        assert_eq!(a.n, 50);
        assert!((a.observed - 0.7).abs() < 1e-12);
        assert!((a.kappa.unwrap() - 0.4).abs() < 1e-12);
        // A judge that passes everything agrees 90% with a 90%-passing human set, and kappa
        // calls it what it is: no better than chance.
        let mut lazy: Vec<(f64, f64)> = vec![(1.0, 1.0); 9];
        lazy.push((1.0, 0.0));
        let a = pass_fail_agreement(&lazy, 0.5).unwrap();
        assert!((a.observed - 0.9).abs() < 1e-12);
        assert_eq!(a.kappa, Some(0.0));
        assert!(pass_fail_agreement(&[], 0.5).is_none());
    }

    fn items(vals: &[(&str, f64)]) -> crate::eval::ItemValues {
        crate::eval::ItemValues {
            evaluator: "judge_x".into(),
            items: vals.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            unkeyed: 0,
        }
    }

    #[test]
    fn stability_over_repeat_runs() {
        let runs = vec![
            (
                "r1".to_string(),
                items(&[("a", 0.9), ("b", 0.2), ("c", 0.6), ("d", 0.8)]),
            ),
            (
                "r2".to_string(),
                items(&[("a", 0.9), ("b", 0.2), ("c", 0.4), ("d", 0.8)]),
            ),
            (
                "r3".to_string(),
                items(&[("a", 0.9), ("b", 0.3), ("c", 0.6)]),
            ),
        ];
        let s = stability(&runs, 0.5).unwrap().unwrap();
        assert_eq!((s.runs, s.items, s.left_out), (3, 3, 1), "d is not in r3");
        // a and b keep their verdict; c passes, fails, passes.
        assert!((s.verdict_agreement - 2.0 / 3.0).abs() < 1e-12);
        assert!((s.max_range - 0.2).abs() < 1e-12);
        // Per-item SDs: a 0, b √(1/300), c √(4/300); their mean.
        let want = ((1.0_f64 / 300.0).sqrt() + (4.0_f64 / 300.0).sqrt()) / 3.0;
        assert!((s.mean_sd - want).abs() < 1e-12, "{}", s.mean_sd);
        assert_eq!(s.least_stable[0].0, "c");
        assert_eq!(s.least_stable[0].1, vec![0.6, 0.4, 0.6]);
        assert_eq!(s.least_stable.len(), 2, "a never moved");
        // Fleiss over pass/fail: a [3,0], b [0,3], c [2,1] → P̄ = (1 + 1 + 1/3) / 3, p = (5/9,
        // 4/9), P̄e = 41/81, κ = (7/9 − 41/81) / (40/81) = 22/40.
        assert!((s.fleiss_kappa.unwrap() - 0.55).abs() < 1e-12);

        // A perfectly repeatable judge.
        let same = vec![
            ("r1".to_string(), items(&[("a", 0.9), ("b", 0.1)])),
            ("r2".to_string(), items(&[("a", 0.9), ("b", 0.1)])),
        ];
        let s = stability(&same, 0.5).unwrap().unwrap();
        assert_eq!(
            (s.verdict_agreement, s.mean_sd, s.max_range),
            (1.0, 0.0, 0.0)
        );
        assert_eq!(s.fleiss_kappa, Some(1.0));
        assert!(s.least_stable.is_empty());
    }

    #[test]
    fn stability_needs_matchable_runs() {
        assert!(stability(&[("r1".into(), items(&[("a", 1.0)]))], 0.5)
            .unwrap()
            .is_none());
        let disjoint = vec![
            ("r1".to_string(), items(&[("a", 1.0)])),
            ("r2".to_string(), items(&[("b", 1.0)])),
        ];
        assert!(stability(&disjoint, 0.5).unwrap().is_none());
        let mut unkeyed = items(&[("a", 1.0)]);
        unkeyed.unkeyed = 2;
        let err = stability(
            &[("r1".into(), items(&[("a", 1.0)])), ("r2".into(), unkeyed)],
            0.5,
        )
        .unwrap_err();
        assert!(
            format!("{err}").starts_with("run r2: 2 item(s) have no id"),
            "{err}"
        );
    }
}
