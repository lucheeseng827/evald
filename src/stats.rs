//! Dependency-free statistical significance for run-to-run eval comparison.
//!
//! `evald eval compare` diffs two runs' per-evaluator means. A *delta* alone can't tell a
//! real regression from sampling noise — on a 20-item dataset a mean dropping 0.85 → 0.80 is
//! well within what chance produces. This module answers the honest question a regression
//! gate must answer: **is the change larger than the noise?**
//!
//! We use **Welch's two-sample t-test** on the per-item values (unequal variances, unequal
//! n) — it directly tests the quantity the gate cares about (the mean) and works for both
//! binary scorers (`value ∈ {0,1}`, where it reduces to an unpooled two-proportion test) and
//! graded scorers (`value ∈ [0,1]`, e.g. levenshtein), without having to classify the
//! evaluator. Inputs are the *sufficient statistics* persisted per run (n, mean, variance),
//! so no per-item scores need to be re-read.
//!
//! When both runs scored the **same items** (matched by item id), the per-item differences
//! are tested directly instead ([`paired_t_test`], with a percentile [`bootstrap_mean_ci`] as a
//! distribution-free check): item-to-item variation cancels, so the paired test sees a real
//! change that Welch's would call noise. Chance-corrected agreement for judges lives here too:
//! [`cohen_kappa`] (two raters) and [`fleiss_kappa`] (N repeat runs).
//!
//! No `statrs`/`nalgebra` dependency: the Student's-t CDF is computed from the **regularized
//! incomplete beta function** via a Lanczos `ln_gamma` + the Lentz continued fraction
//! (Numerical Recipes §6.4), keeping the lean pure-Rust / clean-static-musl posture. All
//! functions are unit-tested against closed-form references (e.g. the df=1 Cauchy CDF).

/// Lanczos `ln Γ(x)` (g = 7, n = 9 coefficients). Accurate to ~1e-13 for x > 0, which is all
/// we need (the incomplete-beta parameters here are `df/2 ≥ 0.5` and `0.5`).
fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    // Reflection formula for the left half-plane keeps the approximation valid for x < 0.5.
    if x < 0.5 {
        let pi = std::f64::consts::PI;
        (pi / (pi * x).sin()).ln() - ln_gamma(1.0 - x)
    } else {
        let x = x - 1.0;
        let mut a = C[0];
        let t = x + G + 0.5;
        for (i, &c) in C.iter().enumerate().skip(1) {
            a += c / (x + i as f64);
        }
        0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
    }
}

/// The continued fraction for the incomplete beta (Numerical Recipes `betacf`, modified
/// Lentz). Converges for `x < (a+1)/(a+b+2)`; the caller applies the symmetry transform
/// otherwise.
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAXIT: usize = 200;
    const EPS: f64 = 3.0e-14;
    const FPMIN: f64 = 1.0e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAXIT {
        let m = m as f64;
        let m2 = 2.0 * m;
        // even step
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        // odd step
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// The regularized incomplete beta function `I_x(a, b)` ∈ [0, 1] (Numerical Recipes `betai`).
fn reg_inc_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let ln_front = ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln();
    let front = ln_front.exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * betacf(a, b, x) / a
    } else {
        1.0 - front * betacf(b, a, 1.0 - x) / b
    }
}

/// `P(|T| ≥ |t|)` for a Student's-t with `df` degrees of freedom — the two-sided p-value.
/// `= I_{df/(df+t²)}(df/2, 1/2)`.
pub fn student_t_two_sided_p(t: f64, df: f64) -> f64 {
    if df <= 0.0 {
        return f64::NAN;
    }
    if !t.is_finite() {
        return if t.is_nan() { f64::NAN } else { 0.0 };
    }
    let x = df / (df + t * t);
    reg_inc_beta(df / 2.0, 0.5, x).clamp(0.0, 1.0)
}

/// The Student's-t CDF `P(T ≤ t)` with `df` degrees of freedom.
pub fn student_t_cdf(t: f64, df: f64) -> f64 {
    if t == 0.0 {
        return 0.5;
    }
    let ib = student_t_two_sided_p(t, df); // = P(|T| ≥ |t|)
    if t > 0.0 {
        1.0 - 0.5 * ib
    } else {
        0.5 * ib
    }
}

/// The Student's-t quantile (inverse CDF): the `t` such that `P(T ≤ t) = p`, by bisection on
/// the monotone CDF. Used to build the confidence interval. `p` is clamped to (0, 1).
///
/// The bracket is grown outward (not a fixed `[-1e9, 1e9]`) so the result is correct even for
/// heavy-tailed small-`df` distributions whose quantile exceeds any fixed bound — for `df < 1`
/// a fixed bracket would silently saturate and return a grossly wrong critical value.
pub fn student_t_quantile(p: f64, df: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    // Expand `hi` until the CDF reaches `p`, and `lo` until it drops to `p`, so the bracket is
    // guaranteed to straddle the root before bisecting.
    let mut hi = 1.0_f64;
    while hi < f64::MAX / 4.0 && student_t_cdf(hi, df) < p {
        hi *= 2.0;
    }
    let mut lo = -1.0_f64;
    while lo > f64::MIN / 4.0 && student_t_cdf(lo, df) > p {
        lo *= 2.0;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if student_t_cdf(mid, df) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The outcome of a t-test on `(mean_b − mean_a)`: Welch's two-sample test, or the paired test
/// on per-item differences.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TTest {
    /// The t-statistic `(mean_b − mean_a) / se`.
    pub t: f64,
    /// Degrees of freedom: Welch–Satterthwaite (non-integer in general), or `pairs − 1`.
    pub df: f64,
    /// Two-sided p-value `P(|T| ≥ |t|)`.
    pub p_two_sided: f64,
    /// Lower bound of the `(1 − alpha)` confidence interval on `(mean_b − mean_a)`.
    pub ci_low: f64,
    /// Upper bound of the `(1 − alpha)` confidence interval on `(mean_b − mean_a)`.
    pub ci_high: f64,
}

/// The name this result had before the paired test shared it.
pub type WelchResult = TTest;

impl TTest {
    /// The one-sided p-value for the regression direction (B below A). Only meaningful when
    /// `mean_b < mean_a`; the gate calls it after confirming a negative delta.
    pub fn p_one_sided(&self) -> f64 {
        0.5 * self.p_two_sided
    }
}

/// Welch's unequal-variance two-sample t-test for `H0: mean_a == mean_b`, testing the
/// difference `delta = mean_b − mean_a`. `var_*` are the unbiased (ddof = 1) sample
/// variances. Returns `None` when either sample is too small to have a variance (`n < 2`).
///
/// `alpha` sets the confidence level of the returned interval (e.g. 0.05 → 95% CI).
pub fn welch_t_test(
    mean_a: f64,
    var_a: f64,
    n_a: u64,
    mean_b: f64,
    var_b: f64,
    n_b: u64,
    alpha: f64,
) -> Option<WelchResult> {
    if n_a < 2 || n_b < 2 {
        return None;
    }
    let (na, nb) = (n_a as f64, n_b as f64);
    let se2_a = var_a.max(0.0) / na;
    let se2_b = var_b.max(0.0) / nb;
    let se = (se2_a + se2_b).sqrt();
    let delta = mean_b - mean_a;

    // Both samples constant (zero variance). Two cases:
    //   • delta == 0: the runs are identical constants → genuinely "no change" (p = 1).
    //   • delta != 0: the means differ, but with zero spread there is NO variance estimate, so a
    //     t-test is undefined. Returning t = ±∞ / p = 0 would manufacture certainty out of two
    //     identical points (e.g. 2/2 vs 0/2) — exactly the false confidence the significance gate
    //     exists to avoid. Report it untestable (None); the caller then gates the regression on
    //     the raw delta instead of waving it through as "proven significant".
    if se == 0.0 {
        if delta == 0.0 {
            return Some(WelchResult {
                t: 0.0,
                df: na + nb - 2.0,
                p_two_sided: 1.0,
                ci_low: 0.0,
                ci_high: 0.0,
            });
        }
        return None;
    }

    let t = delta / se;
    // Welch–Satterthwaite degrees of freedom.
    let df = (se2_a + se2_b).powi(2) / (se2_a.powi(2) / (na - 1.0) + se2_b.powi(2) / (nb - 1.0));
    let p_two_sided = student_t_two_sided_p(t, df);
    let t_crit = student_t_quantile(1.0 - alpha / 2.0, df);
    let half = t_crit * se;
    Some(WelchResult {
        t,
        df,
        p_two_sided,
        ci_low: delta - half,
        ci_high: delta + half,
    })
}

/// The paired t-test on per-item differences `diffs[i] = b_i − a_i` (the same items scored in
/// both runs), for `H0: mean difference == 0`. Returns `None` for fewer than 2 pairs, and, as
/// [`welch_t_test`] does, when every difference is the same non-zero value: with no spread
/// there is no variance estimate, and certainty would be manufactured.
pub fn paired_t_test(diffs: &[f64], alpha: f64) -> Option<TTest> {
    let n = diffs.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mean = diffs.iter().sum::<f64>() / nf;
    let var = diffs.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / (nf - 1.0);
    let se = (var / nf).sqrt();
    let df = nf - 1.0;
    if se == 0.0 {
        return (mean == 0.0).then_some(TTest {
            t: 0.0,
            df,
            p_two_sided: 1.0,
            ci_low: 0.0,
            ci_high: 0.0,
        });
    }
    let t = mean / se;
    let half = student_t_quantile(1.0 - alpha / 2.0, df) * se;
    Some(TTest {
        t,
        df,
        p_two_sided: student_t_two_sided_p(t, df),
        ci_low: mean - half,
        ci_high: mean + half,
    })
}

/// What a non-inferiority test concluded about candidate B against baseline A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInferiorVerdict {
    /// B is no worse than A by more than the margin: the one-sided lower bound on `b − a` is
    /// above `−margin`.
    NonInferior,
    /// B is worse than A by more than the margin: even the upper bound is below `−margin`.
    Inferior,
    /// Neither shown: the data cannot tell. A small dataset lands here, which is the point:
    /// no significant difference is not evidence of no difference.
    Inconclusive,
}

impl NonInferiorVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            NonInferiorVerdict::NonInferior => "non-inferior",
            NonInferiorVerdict::Inferior => "inferior",
            NonInferiorVerdict::Inconclusive => "inconclusive",
        }
    }
}

/// A one-sided paired non-inferiority test on `diffs[i] = b_i − a_i`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NonInferiority {
    pub pairs: usize,
    /// The mean difference `b − a`.
    pub mean: f64,
    /// The one-sided `(1 − alpha)` lower confidence bound on the mean difference.
    pub lower: f64,
    /// The one-sided `(1 − alpha)` upper confidence bound.
    pub upper: f64,
    /// The p-value for `H0: mean ≤ −margin` (B worse by at least the margin).
    pub p: f64,
    pub verdict: NonInferiorVerdict,
}

/// Is B no worse than A by more than `margin`? The paired test of `H0: μ ≤ −margin` against
/// `H1: μ > −margin` on the per-item differences, at one-sided level `alpha`: B is non-inferior
/// when the one-sided `(1 − alpha)` lower bound `mean − t(1 − alpha, n − 1)·se` is above
/// `−margin`, and inferior when the upper bound is below it. `None` for fewer than 2 pairs, and
/// when every difference is the same non-zero value (no variance estimate, as
/// [`paired_t_test`]); identical runs (every difference 0) are non-inferior for any margin.
pub fn non_inferiority(diffs: &[f64], margin: f64, alpha: f64) -> Option<NonInferiority> {
    let n = diffs.len();
    if n < 2 || !margin.is_finite() || margin <= 0.0 {
        return None;
    }
    let nf = n as f64;
    let mean = diffs.iter().sum::<f64>() / nf;
    let var = diffs.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / (nf - 1.0);
    let se = (var / nf).sqrt();
    let df = nf - 1.0;
    let (lower, upper, p) = if se == 0.0 {
        if mean != 0.0 {
            return None;
        }
        (0.0, 0.0, 0.0)
    } else {
        let half = student_t_quantile(1.0 - alpha, df) * se;
        let t = (mean + margin) / se;
        (mean - half, mean + half, 1.0 - student_t_cdf(t, df))
    };
    let verdict = if lower > -margin {
        NonInferiorVerdict::NonInferior
    } else if upper < -margin {
        NonInferiorVerdict::Inferior
    } else {
        NonInferiorVerdict::Inconclusive
    };
    Some(NonInferiority {
        pairs: n,
        mean,
        lower,
        upper,
        p,
        verdict,
    })
}

/// SplitMix64: a small, seedable generator, so a bootstrap interval is the same on every run.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (Lemire's multiply-shift; the bias is far below what a CI can show).
    fn below(&mut self, n: usize) -> usize {
        ((self.next() as u128 * n as u128) >> 64) as usize
    }
}

/// How many resamples [`bootstrap_mean_ci`] draws when the caller has no reason to choose.
pub const BOOTSTRAP_RESAMPLES: usize = 2_000;

/// A percentile bootstrap `(1 − alpha)` interval on the mean of `values`: `resamples` draws of
/// `values.len()` items with replacement, seeded by `seed` so the interval is reproducible. It
/// assumes nothing about the distribution, so it is the check on a paired t interval for
/// scores that are far from normal (pass/fail, a few discrete levels). `None` for fewer than 2
/// values.
pub fn bootstrap_mean_ci(
    values: &[f64],
    alpha: f64,
    resamples: usize,
    seed: u64,
) -> Option<(f64, f64)> {
    let n = values.len();
    if n < 2 || resamples == 0 {
        return None;
    }
    let mut rng = SplitMix64(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| values[rng.below(n)]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let at = |q: f64| means[((q * resamples as f64).floor() as usize).min(resamples - 1)];
    Some((at(alpha / 2.0), at(1.0 - alpha / 2.0)))
}

/// Cohen's kappa: agreement between two raters on the same items, corrected for the agreement
/// their own label frequencies would produce by chance. `pairs[i]` is (rater A's label, rater
/// B's label) for item `i`. `None` with no items, or when chance agreement is already total
/// (both raters gave every item the same one label), where kappa is undefined.
pub fn cohen_kappa<T: Ord + Clone>(pairs: &[(T, T)]) -> Option<f64> {
    use std::collections::BTreeMap;
    let n = pairs.len();
    if n == 0 {
        return None;
    }
    let nf = n as f64;
    let mut a: BTreeMap<T, f64> = BTreeMap::new();
    let mut b: BTreeMap<T, f64> = BTreeMap::new();
    let mut agree = 0.0;
    for (x, y) in pairs {
        *a.entry(x.clone()).or_default() += 1.0;
        *b.entry(y.clone()).or_default() += 1.0;
        if x == y {
            agree += 1.0;
        }
    }
    let p_o = agree / nf;
    let p_e: f64 = a
        .iter()
        .map(|(label, ca)| ca / nf * b.get(label).copied().unwrap_or(0.0) / nf)
        .sum();
    if (1.0 - p_e).abs() < 1e-12 {
        return None;
    }
    Some((p_o - p_e) / (1.0 - p_e))
}

/// Fleiss' kappa: agreement among the same number of raters on every item (here: N repeat runs
/// of one judge). `counts[i][j]` is how many raters put item `i` in category `j`. `None` with no
/// items, fewer than 2 raters, unequal rater counts, or total chance agreement.
pub fn fleiss_kappa(counts: &[Vec<u64>]) -> Option<f64> {
    let first = counts.first()?;
    let raters: u64 = first.iter().sum();
    if raters < 2
        || counts
            .iter()
            .any(|row| row.len() != first.len() || row.iter().sum::<u64>() != raters)
    {
        return None;
    }
    let (n, r) = (counts.len() as f64, raters as f64);
    let p_bar = counts
        .iter()
        .map(|row| {
            let s: f64 = row.iter().map(|&c| (c * c) as f64).sum();
            (s - r) / (r * (r - 1.0))
        })
        .sum::<f64>()
        / n;
    let p_e: f64 = (0..first.len())
        .map(|j| {
            let pj = counts.iter().map(|row| row[j] as f64).sum::<f64>() / (n * r);
            pj * pj
        })
        .sum();
    if (1.0 - p_e).abs() < 1e-12 {
        return None;
    }
    Some((p_bar - p_e) / (1.0 - p_e))
}

/// Landis & Koch's words for a kappa, the scale most papers quote.
pub fn kappa_words(k: f64) -> &'static str {
    match k {
        k if k < 0.0 => "worse than chance",
        k if k <= 0.20 => "slight",
        k if k <= 0.40 => "fair",
        k if k <= 0.60 => "moderate",
        k if k <= 0.80 => "substantial",
        _ => "almost perfect",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n/2` items moved up by `spread` and `n/2` down, all shifted by `shift`: the mean
    /// difference is `shift` and the sample variance is `spread² · n / (n − 1)`, exactly.
    fn diffs(n: usize, spread: f64, shift: f64) -> Vec<f64> {
        (0..n)
            .map(|i| shift + if i % 2 == 0 { spread } else { -spread })
            .collect()
    }

    #[test]
    fn non_inferiority_known_answers() {
        // t(0.95, 99) = 1.660391 (tables); se = 0.1 · sqrt(100/99) / 10 = 0.0100504.
        assert!((student_t_quantile(0.95, 99.0) - 1.660391).abs() < 1e-5);
        let se = 0.1 * (100.0f64 / 99.0).sqrt() / 10.0;
        let half = 1.660391 * se; // 0.016688

        // B equal to A on average, items moving ±0.1: no worse by 0.02, but not shown no worse
        // by 0.01.
        let d = diffs(100, 0.1, 0.0);
        let r = non_inferiority(&d, 0.02, 0.05).unwrap();
        assert!((r.mean).abs() < 1e-12);
        assert!((r.lower + half).abs() < 1e-5, "{r:?}");
        assert!((r.upper - half).abs() < 1e-5, "{r:?}");
        assert_eq!(r.verdict, NonInferiorVerdict::NonInferior);
        // p = P(T > 0.02/se) = P(T > 1.98997) on 99 df ≈ 0.0247.
        assert!((r.p - 0.0247).abs() < 5e-4, "{r:?}");
        assert!(r.p < 0.05);
        assert_eq!(
            non_inferiority(&d, 0.01, 0.05).unwrap().verdict,
            NonInferiorVerdict::Inconclusive
        );

        // B 0.06 below A: worse by more than 0.02, and shown to be (upper −0.0433 < −0.02).
        let r = non_inferiority(&diffs(100, 0.1, -0.06), 0.02, 0.05).unwrap();
        assert!((r.upper - (-0.06 + half)).abs() < 1e-5, "{r:?}");
        assert_eq!(r.verdict, NonInferiorVerdict::Inferior);
        assert!(r.p > 0.99);

        // A lower alpha asks for more: the bound widens to t(0.975, 99) = 1.984217.
        let strict = non_inferiority(&d, 0.02, 0.025).unwrap();
        assert!((strict.lower + 1.984217 * se).abs() < 1e-5, "{strict:?}");
        assert_eq!(strict.verdict, NonInferiorVerdict::NonInferior);
    }

    #[test]
    fn a_small_dataset_with_no_significant_difference_is_inconclusive_not_non_inferior() {
        // Eight items: two got worse, one better. The two-sided test calls it noise...
        let d = [0.0, 0.0, -1.0, 0.0, 1.0, 0.0, -1.0, 0.0];
        let t = paired_t_test(&d, 0.05).unwrap();
        assert!(t.p_two_sided > 0.5, "{t:?}");
        assert!(
            t.ci_low < 0.0 && t.ci_high > 0.0,
            "no significant difference"
        );
        // ...which is absence of evidence: with a 0.05 margin it shows neither.
        let r = non_inferiority(&d, 0.05, 0.05).unwrap();
        assert_eq!(r.verdict, NonInferiorVerdict::Inconclusive, "{r:?}");
        // mean −0.125, sd 0.640870, se 0.226583, t(0.95, 7) = 1.894579.
        assert!(
            (r.lower - (-0.125 - 1.894579 * 0.226583)).abs() < 1e-4,
            "{r:?}"
        );
    }

    #[test]
    fn non_inferiority_edge_cases() {
        // Identical runs: no worse by any margin.
        let same = non_inferiority(&[0.0; 10], 0.01, 0.05).unwrap();
        assert_eq!(same.verdict, NonInferiorVerdict::NonInferior);
        // Every item moved by the same amount: no variance, no test.
        assert_eq!(non_inferiority(&[-0.5; 10], 0.01, 0.05), None);
        assert_eq!(non_inferiority(&[0.1], 0.01, 0.05), None, "one pair");
        assert_eq!(
            non_inferiority(&[0.1, 0.2], 0.0, 0.05),
            None,
            "a margin is required"
        );
        assert_eq!(non_inferiority(&[0.1, 0.2], f64::NAN, 0.05), None);
    }

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn ln_gamma_known_values() {
        assert!(close(ln_gamma(1.0), 0.0, 1e-10)); // Γ(1)=1
        assert!(close(ln_gamma(2.0), 0.0, 1e-10)); // Γ(2)=1
        assert!(close(ln_gamma(5.0), 24.0_f64.ln(), 1e-9)); // Γ(5)=4!=24
        assert!(close(
            ln_gamma(0.5),
            std::f64::consts::PI.sqrt().ln(),
            1e-10
        )); // Γ(½)=√π
    }

    #[test]
    fn reg_inc_beta_symmetric_half() {
        // I_0.5(0.5, 0.5) = (2/π)·arcsin(√0.5) = 0.5 exactly.
        assert!(close(reg_inc_beta(0.5, 0.5, 0.5), 0.5, 1e-12));
        assert_eq!(reg_inc_beta(2.0, 3.0, 0.0), 0.0);
        assert_eq!(reg_inc_beta(2.0, 3.0, 1.0), 1.0);
        // I_x(a,b) + I_{1-x}(b,a) = 1 (reflection).
        assert!(close(
            reg_inc_beta(2.0, 5.0, 0.3) + reg_inc_beta(5.0, 2.0, 0.7),
            1.0,
            1e-12
        ));
    }

    #[test]
    fn student_t_cdf_matches_cauchy_closed_form() {
        // df = 1 is the standard Cauchy: F(t) = 0.5 + atan(t)/π.
        for &t in &[-3.0_f64, -1.0, -0.3, 0.0, 0.3, 1.0, 3.0] {
            let want = 0.5 + t.atan() / std::f64::consts::PI;
            assert!(
                close(student_t_cdf(t, 1.0), want, 1e-9),
                "cdf({t},1) = {} want {want}",
                student_t_cdf(t, 1.0)
            );
        }
        assert!(close(student_t_cdf(0.0, 10.0), 0.5, 1e-12));
        // Large df → standard normal: Φ(1.96) ≈ 0.975.
        assert!(close(student_t_cdf(1.96, 1.0e7), 0.975, 1e-3));
    }

    #[test]
    fn two_sided_p_is_tail_mass() {
        // df=1, t=1: P(|T|≥1) = 2·(1−0.75) = 0.5.
        assert!(close(student_t_two_sided_p(1.0, 1.0), 0.5, 1e-9));
        // p and cdf are consistent: P(|T|≥t) = 2·(1 − F(|t|)).
        let p = student_t_two_sided_p(2.2, 8.0);
        let f = student_t_cdf(2.2, 8.0);
        assert!(close(p, 2.0 * (1.0 - f), 1e-9));
    }

    #[test]
    fn quantile_inverts_cdf() {
        // Cauchy (df=1) quantile is tan(π(p−0.5)); Q(0.95)=tan(0.45π)≈6.3138.
        assert!(close(
            student_t_quantile(0.95, 1.0),
            (0.45 * std::f64::consts::PI).tan(),
            1e-4
        ));
        // Standard 95% two-sided critical values.
        assert!(close(student_t_quantile(0.975, 10.0), 2.228, 2e-3));
        assert!(close(student_t_quantile(0.975, 1.0e7), 1.96, 2e-3));
        // Round-trip, including a heavy-tailed df < 1.
        for &df in &[0.5, 2.0, 5.0, 30.0, 200.0] {
            let q = student_t_quantile(0.9, df);
            assert!(close(student_t_cdf(q, df), 0.9, 1e-6), "df={df} q={q}");
        }
        // df=0.1: the 0.975 quantile exceeds the old fixed 1e9 bracket — the adaptive bracket
        // must grow past it and still invert the CDF (a fixed bracket silently saturated here).
        let q = student_t_quantile(0.975, 0.1);
        assert!(
            q > 1.0e9,
            "heavy tail -> quantile past the old fixed bound, got {q}"
        );
        assert!(
            close(student_t_cdf(q, 0.1), 0.975, 1e-3),
            "cdf(q,0.1)={}",
            student_t_cdf(q, 0.1)
        );
    }

    #[test]
    fn welch_worked_example_is_significant() {
        // Two binary scorers: A = 90/100 pass, B = 80/100. Bernoulli sample variance
        // (ddof=1) = p(1−p)·n/(n−1).
        let var = |p: f64| p * (1.0 - p) * 100.0 / 99.0;
        let r = welch_t_test(0.90, var(0.90), 100, 0.80, var(0.80), 100, 0.05).unwrap();
        assert!(r.t < 0.0, "B is worse so t < 0, got {}", r.t);
        assert!(close(r.t, -1.99, 0.05), "t≈-1.99, got {}", r.t);
        assert!(r.df > 150.0 && r.df < 220.0, "Welch df≈184, got {}", r.df);
        assert!(r.p_two_sided < 0.05, "p≈0.048, got {}", r.p_two_sided);
        assert!(
            r.ci_high < 0.0,
            "95% CI on delta excludes 0: {:?}",
            (r.ci_low, r.ci_high)
        );
    }

    #[test]
    fn welch_small_n_difference_is_noise() {
        // 0.85 vs 0.80 on n=20 each — a real-looking delta that is NOT significant.
        let var = |p: f64| p * (1.0 - p) * 20.0 / 19.0;
        let r = welch_t_test(0.85, var(0.85), 20, 0.80, var(0.80), 20, 0.05).unwrap();
        assert!(r.p_two_sided > 0.05, "should be noise, p={}", r.p_two_sided);
        assert!(
            r.ci_low < 0.0 && r.ci_high > 0.0,
            "CI straddles 0: {:?}",
            (r.ci_low, r.ci_high)
        );
    }

    #[test]
    fn welch_insufficient_sample_returns_none() {
        assert!(welch_t_test(1.0, 0.0, 1, 0.5, 0.1, 10, 0.05).is_none());
        assert!(welch_t_test(1.0, 0.1, 10, 0.5, 0.0, 1, 0.05).is_none());
    }

    #[test]
    fn welch_n2_exact_is_testable() {
        // n=2 each is the smallest sample the gate ever tests: df comes from (n-1)=1, the
        // heaviest-tailed valid case. Must return Some with finite, sane outputs.
        let r = welch_t_test(0.7, 0.02, 2, 0.5, 0.03, 2, 0.05).unwrap();
        assert!(r.df.is_finite() && r.df > 0.0, "df={}", r.df);
        assert!(
            r.ci_low < r.ci_high,
            "a real interval: {:?}",
            (r.ci_low, r.ci_high)
        );
        assert!(r.p_two_sided.is_finite() && (0.0..=1.0).contains(&r.p_two_sided));
    }

    #[test]
    fn welch_identical_means_with_variance_is_centered() {
        // The common "rerun, nothing changed" case (delta=0, real variance): t=0, p≈1, and the
        // CI is symmetric about 0. A sign error in the half-width or t would surface here.
        let r = welch_t_test(0.8, 0.16, 30, 0.8, 0.16, 30, 0.05).unwrap();
        assert_eq!(r.t, 0.0);
        assert!(close(r.p_two_sided, 1.0, 1e-9), "p={}", r.p_two_sided);
        assert!(
            close(r.ci_low + r.ci_high, 0.0, 1e-9),
            "CI centered on 0: {:?}",
            (r.ci_low, r.ci_high)
        );
    }

    #[test]
    fn welch_ci_widens_as_alpha_shrinks() {
        // alpha is consumed only via student_t_quantile(1 - alpha/2, df); a tighter alpha must
        // widen the interval. Guards against an inverted quantile or a 1-alpha mix-up.
        let width = |alpha: f64| {
            let r = welch_t_test(0.9, 0.05, 40, 0.8, 0.05, 40, alpha).unwrap();
            r.ci_high - r.ci_low
        };
        let (w50, w05, w001) = (width(0.5), width(0.05), width(0.001));
        assert!(
            w50 < w05 && w05 < w001,
            "CI must widen as alpha shrinks: {w50} {w05} {w001}"
        );
    }

    #[test]
    fn welch_zero_variance_cases() {
        // Both constant, same mean → genuinely no change.
        let same = welch_t_test(0.9, 0.0, 5, 0.9, 0.0, 5, 0.05).unwrap();
        assert_eq!(same.p_two_sided, 1.0);
        assert_eq!((same.ci_low, same.ci_high), (0.0, 0.0));
        // Both constant but DIFFERENT mean → no variance estimate exists, so the t-test is
        // undefined. We must NOT manufacture p = 0 / t = -∞ "certainty" from two identical points;
        // the test is untestable (None) and the caller falls back to the raw-delta gate.
        assert!(
            welch_t_test(1.0, 0.0, 5, 0.0, 0.0, 5, 0.05).is_none(),
            "constant-but-different samples must be untestable, not falsely 'certain'"
        );
    }

    /// Five items, each a little worse in run B, on items that vary a lot among themselves.
    fn shifted() -> (Vec<f64>, Vec<f64>) {
        let a = vec![0.6, 0.7, 0.8, 0.5, 0.9];
        let drop = [0.1, 0.05, 0.12, 0.08, 0.1];
        let b = a.iter().zip(drop).map(|(x, d)| x - d).collect();
        (a, b)
    }

    fn mean_var(v: &[f64]) -> (f64, f64) {
        let n = v.len() as f64;
        let m = v.iter().sum::<f64>() / n;
        (
            m,
            v.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1.0),
        )
    }

    #[test]
    fn paired_sees_a_consistent_drop_that_welch_calls_noise() {
        let (a, b) = shifted();
        let diffs: Vec<f64> = a.iter().zip(&b).map(|(x, y)| y - x).collect();
        let paired = paired_t_test(&diffs, 0.05).unwrap();
        // By hand: d̄ = −0.09, s_d² = 0.0028 / 4 = 0.0007, se = √(0.0007 / 5), t = d̄ / se.
        assert!(close(paired.t, -0.09 / (0.0007_f64 / 5.0).sqrt(), 1e-9));
        assert!(close(paired.t, -7.6064, 1e-3));
        assert_eq!(paired.df, 4.0);
        // df = 4 has a closed form: P(|T| < t) = sin θ · (1 + cos²θ / 2), θ = atan(t / 2).
        let th = (paired.t.abs() / 2.0).atan();
        let want_p = 1.0 - th.sin() * (1.0 + th.cos().powi(2) / 2.0);
        assert!(
            close(paired.p_two_sided, want_p, 1e-9),
            "{}",
            paired.p_two_sided
        );
        assert!(paired.ci_high < 0.0, "the paired CI excludes 0");

        let ((ma, va), (mb, vb)) = (mean_var(&a), mean_var(&b));
        let welch = welch_t_test(ma, va, 5, mb, vb, 5, 0.05).unwrap();
        assert!(close(
            welch.t,
            (mb - ma) / (va / 5.0 + vb / 5.0).sqrt(),
            1e-12
        ));
        assert!(welch.p_two_sided > 0.3, "Welch: {}", welch.p_two_sided);
        assert!(
            welch.ci_low < 0.0 && welch.ci_high > 0.0,
            "Welch's CI spans 0"
        );
    }

    #[test]
    fn paired_edge_cases() {
        assert!(paired_t_test(&[0.1], 0.05).is_none());
        let none = paired_t_test(&[0.0, 0.0, 0.0], 0.05).unwrap();
        assert_eq!(
            (none.p_two_sided, none.ci_low, none.ci_high),
            (1.0, 0.0, 0.0)
        );
        assert!(
            paired_t_test(&[-1.0, -1.0, -1.0], 0.05).is_none(),
            "a constant non-zero shift has no variance estimate"
        );
    }

    #[test]
    fn bootstrap_is_reproducible_and_brackets_the_mean() {
        let (a, b) = shifted();
        let diffs: Vec<f64> = a.iter().zip(&b).map(|(x, y)| y - x).collect();
        let ci = bootstrap_mean_ci(&diffs, 0.05, BOOTSTRAP_RESAMPLES, 7).unwrap();
        assert_eq!(
            ci,
            bootstrap_mean_ci(&diffs, 0.05, BOOTSTRAP_RESAMPLES, 7).unwrap()
        );
        assert!(ci.0 <= -0.09 && -0.09 <= ci.1, "{ci:?}");
        assert!(
            ci.1 < 0.0,
            "every resample of a uniform drop is a drop: {ci:?}"
        );
        // Inside the observed range, always.
        assert!(ci.0 >= -0.12 && ci.1 <= -0.05, "{ci:?}");
        assert_eq!(
            bootstrap_mean_ci(&[0.25; 4], 0.05, 100, 1),
            Some((0.25, 0.25))
        );
        assert!(bootstrap_mean_ci(&[0.1], 0.05, 100, 1).is_none());
        // Pass/fail differences: half the items flipped down, half unchanged.
        let flips: Vec<f64> = (0..40)
            .map(|i| if i % 2 == 0 { -1.0 } else { 0.0 })
            .collect();
        let (lo, hi) = bootstrap_mean_ci(&flips, 0.05, BOOTSTRAP_RESAMPLES, 3).unwrap();
        assert!(lo < -0.5 && hi > -0.5 && hi < 0.0, "({lo}, {hi})");
    }

    #[test]
    fn cohen_kappa_matches_a_hand_computed_table() {
        // 50 items: yes/yes 20, yes/no 5, no/yes 10, no/no 15. p_o = 0.7, p_e = 0.5·0.6 +
        // 0.5·0.4 = 0.5, so κ = (0.7 − 0.5) / (1 − 0.5) = 0.4.
        let mut pairs = Vec::new();
        for (x, y, n) in [(1, 1, 20), (1, 0, 5), (0, 1, 10), (0, 0, 15)] {
            pairs.extend(std::iter::repeat_n((x, y), n));
        }
        assert!(close(cohen_kappa(&pairs).unwrap(), 0.4, 1e-12));
        // Perfect agreement is 1; agreeing exactly as often as chance is 0.
        assert_eq!(cohen_kappa(&[(0, 0), (1, 1), (1, 1)]), Some(1.0));
        assert!(close(
            cohen_kappa(&[(1, 1), (1, 0), (0, 1), (0, 0)]).unwrap(),
            0.0,
            1e-12
        ));
        // Both raters said "pass" to everything: chance explains it all, κ is undefined.
        assert_eq!(cohen_kappa(&[(1, 1), (1, 1)]), None);
        assert_eq!(cohen_kappa::<u8>(&[]), None);
    }

    #[test]
    fn fleiss_kappa_matches_the_worked_example() {
        // Fleiss (1971) as worked on Wikipedia: 10 items, 14 raters, 5 categories → κ ≈ 0.210.
        let table: Vec<Vec<u64>> = [
            [0, 0, 0, 0, 14],
            [0, 2, 6, 4, 2],
            [0, 0, 3, 5, 6],
            [0, 3, 9, 2, 0],
            [2, 2, 8, 1, 1],
            [7, 7, 0, 0, 0],
            [3, 2, 6, 3, 0],
            [2, 5, 3, 2, 2],
            [6, 5, 2, 1, 0],
            [0, 2, 2, 3, 7],
        ]
        .iter()
        .map(|r| r.to_vec())
        .collect();
        assert!(close(fleiss_kappa(&table).unwrap(), 0.210, 5e-4));
        // Three runs that always agree, on items that differ: 1.
        assert_eq!(
            fleiss_kappa(&[vec![3, 0], vec![0, 3], vec![3, 0]]),
            Some(1.0)
        );
        // Unequal rater counts, one rater, or no spread at all: undefined.
        assert_eq!(fleiss_kappa(&[vec![3, 0], vec![1, 1]]), None);
        assert_eq!(fleiss_kappa(&[vec![1, 0]]), None);
        assert_eq!(fleiss_kappa(&[vec![3, 0], vec![3, 0]]), None);
    }

    #[test]
    fn kappa_words_follow_landis_and_koch() {
        assert_eq!(kappa_words(-0.1), "worse than chance");
        assert_eq!(kappa_words(0.4), "fair");
        assert_eq!(kappa_words(0.62), "substantial");
        assert_eq!(kappa_words(0.95), "almost perfect");
    }
}
