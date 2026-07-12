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

/// The outcome of Welch's two-sample t-test on `(mean_b − mean_a)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WelchResult {
    /// The t-statistic `(mean_b − mean_a) / se`.
    pub t: f64,
    /// Welch–Satterthwaite degrees of freedom (non-integer in general).
    pub df: f64,
    /// Two-sided p-value `P(|T| ≥ |t|)`.
    pub p_two_sided: f64,
    /// Lower bound of the `(1 − alpha)` confidence interval on `(mean_b − mean_a)`.
    pub ci_low: f64,
    /// Upper bound of the `(1 − alpha)` confidence interval on `(mean_b − mean_a)`.
    pub ci_high: f64,
}

impl WelchResult {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
