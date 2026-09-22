//! promptfoo-style declarative eval **suite** over the existing engine (top-20 #7).
//!
//! A suite lists eval-config cases and gates on a *suite-level* pass rate, mirroring the promptfoo
//! ergonomics teams expect — suite pass %, fail-on-threshold, and `repeat`/`min_pass` — as a **thin
//! declarative front-end** over evald's existing [`crate::eval::run_eval`] (no new scoring engine).
//! Each case's own thresholds still gate it; the suite adds `repeat` + `min_pass` (a case must pass
//! a fraction of its repeats — the honest gate for non-deterministic judges, a no-op for the
//! deterministic Tier-1 scorers) and `min_pass_rate` (a fraction of cases must pass).
//!
//! This is *promptfoo-style*, not a drop-in parser for promptfoo's `prompts`/`providers`/`assert`
//! schema — evald scores captured outputs rather than calling providers, so the suite composes
//! evald eval configs. Suites gate on Tier-1 evaluators; judge (Tier-3) scoring stays on `eval run`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::eval::{load_config, load_dataset, resolve_relative, run_eval};

/// A declarative suite (`evald suite run <suite.yaml>`).
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteConfig {
    #[serde(default)]
    pub name: Option<String>,
    /// The eval-config cases that make up the suite.
    pub cases: Vec<SuiteCase>,
    /// Fraction of cases that must pass for the suite to pass (`0.0`–`1.0`). Default `1.0` (all).
    #[serde(default = "one")]
    pub min_pass_rate: f64,
    /// Run each case this many times. Default `1`. (>1 is meaningful for non-deterministic judges;
    /// for the deterministic Tier-1 scorers every repeat is identical.)
    #[serde(default = "one_usize")]
    pub repeat: usize,
    /// Fraction of a case's repeats that must pass for the case to pass (`0.0`–`1.0`). Default `1.0`.
    #[serde(default = "one")]
    pub min_pass: f64,
}

/// One suite case: a reference to an eval config.
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteCase {
    #[serde(default)]
    pub name: Option<String>,
    /// Path to an eval config (dataset + evaluators + thresholds), relative to the suite file's dir.
    pub config: PathBuf,
}

fn one() -> f64 {
    1.0
}
fn one_usize() -> usize {
    1
}

/// The result of one case across its repeats.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseResult {
    pub name: String,
    pub runs_total: usize,
    pub runs_passed: usize,
    pub passed: bool,
}

/// The suite verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct SuiteOutcome {
    pub cases: Vec<CaseResult>,
    /// Fraction of cases that passed.
    pub pass_rate: f64,
    /// The suite gate this outcome was judged against (`min_pass_rate`), so a report can say
    /// what the pass rate was compared with.
    pub min_pass_rate: f64,
    pub passed: bool,
}

/// Pure aggregation: given per-case results and the suite gate, compute the pass rate + verdict.
/// A tiny epsilon makes the `>=` gate robust to float division (e.g. `2/3` vs `min_pass_rate=0.66`).
pub fn aggregate(cases: Vec<CaseResult>, min_pass_rate: f64) -> SuiteOutcome {
    let total = cases.len();
    let passed_cases = cases.iter().filter(|c| c.passed).count();
    let pass_rate = if total == 0 {
        1.0
    } else {
        passed_cases as f64 / total as f64
    };
    let passed = pass_rate + 1e-9 >= min_pass_rate;
    SuiteOutcome {
        cases,
        pass_rate,
        min_pass_rate,
        passed,
    }
}

/// Decide whether a case passed from its repeat tally + `min_pass`.
fn case_passed(runs_passed: usize, runs_total: usize, min_pass: f64) -> bool {
    if runs_total == 0 {
        return false;
    }
    (runs_passed as f64 / runs_total as f64) + 1e-9 >= min_pass
}

/// Load + run a suite. Each case runs `repeat` times through [`run_eval`]; a case passes when the
/// fraction of passing repeats `>= min_pass`. Returns the suite outcome. Does **not** persist scores
/// (a suite is a gate, not a run of record — use `eval run` to persist).
pub fn run_suite(path: &Path) -> anyhow::Result<SuiteOutcome> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading suite {}: {e}", path.display()))?;
    let suite: SuiteConfig = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing suite {}: {e}", path.display()))?;
    anyhow::ensure!(!suite.cases.is_empty(), "suite has no cases");
    // A threshold outside [0.0, 1.0] quietly defeats the gate this feature exists to provide:
    // > 1.0 makes the suite unconditionally fail, and a negative value makes it vacuously pass.
    anyhow::ensure!(
        (0.0..=1.0).contains(&suite.min_pass_rate),
        "min_pass_rate must be within [0.0, 1.0], got {}",
        suite.min_pass_rate
    );
    anyhow::ensure!(
        (0.0..=1.0).contains(&suite.min_pass),
        "min_pass must be within [0.0, 1.0], got {}",
        suite.min_pass
    );
    let repeat = suite.repeat.max(1);

    let mut results = Vec::with_capacity(suite.cases.len());
    for (i, case) in suite.cases.iter().enumerate() {
        // Case config paths resolve against the suite file's dir (via `resolve_relative`, which uses
        // the given file's parent); the config's own dataset path then resolves against the config.
        let cfg_path = resolve_relative(path, &case.config);
        let config = load_config(&cfg_path)?;
        let dataset_path = resolve_relative(&cfg_path, &config.dataset);
        let dataset = load_dataset(&dataset_path)?;
        anyhow::ensure!(
            !dataset.is_empty(),
            "case {} dataset {} has no items",
            i,
            dataset_path.display()
        );

        let mut runs_passed = 0usize;
        for r in 0..repeat {
            // `ts=0` / a throwaway run id: the suite is a gate, so the produced Scores are discarded.
            let (report, _scores) = run_eval(&config, &dataset, &format!("suite-{i}-{r}"), 0)?;
            if report.passed() {
                runs_passed += 1;
            }
        }
        results.push(CaseResult {
            name: case
                .name
                .clone()
                .unwrap_or_else(|| cfg_path.display().to_string()),
            runs_total: repeat,
            runs_passed,
            passed: case_passed(runs_passed, repeat, suite.min_pass),
        });
    }
    Ok(aggregate(results, suite.min_pass_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, passed: usize, total: usize, min_pass: f64) -> CaseResult {
        CaseResult {
            name: name.into(),
            runs_total: total,
            runs_passed: passed,
            passed: case_passed(passed, total, min_pass),
        }
    }

    #[test]
    fn case_passes_when_repeat_pass_fraction_meets_min_pass() {
        assert!(case_passed(4, 4, 1.0)); // all pass, strict
        assert!(!case_passed(3, 4, 1.0)); // one fail under strict
        assert!(case_passed(3, 4, 0.75)); // 0.75 >= 0.75
        assert!(!case_passed(2, 4, 0.75)); // 0.5 < 0.75
        assert!(!case_passed(0, 0, 1.0)); // no runs → not passed
    }

    #[test]
    fn suite_gate_uses_case_pass_rate_against_min_pass_rate() {
        let cases = vec![
            case("a", 1, 1, 1.0), // passed
            case("b", 1, 1, 1.0), // passed
            case("c", 0, 1, 1.0), // failed
        ];
        // 2/3 cases pass.
        let strict = aggregate(cases.clone(), 1.0);
        assert!(!strict.passed);
        assert!((strict.pass_rate - 2.0 / 3.0).abs() < 1e-9);

        let lenient = aggregate(cases, 0.66);
        assert!(lenient.passed, "2/3 >= 0.66 should pass");
    }

    #[test]
    fn empty_suite_aggregate_is_vacuously_passing() {
        let out = aggregate(vec![], 1.0);
        assert!(out.passed);
        assert_eq!(out.pass_rate, 1.0);
    }

    /// Write a minimal suite YAML with the given `min_pass_rate`/`min_pass` overrides (as raw
    /// YAML fragments) and a case pointing at a config that doesn't need to exist — the
    /// threshold validation in `run_suite` must reject the file before ever trying to load it.
    fn suite_yaml(dir: &Path, extra: &str) -> PathBuf {
        let path = dir.join("suite.yaml");
        std::fs::write(
            &path,
            format!("cases:\n  - config: nonexistent.yaml\n{extra}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn run_suite_rejects_out_of_range_min_pass_rate() {
        let dir = tempfile::tempdir().unwrap();
        let path = suite_yaml(dir.path(), "min_pass_rate: 1.5\n");
        let err = run_suite(&path).expect_err("min_pass_rate > 1.0 must be rejected");
        assert!(
            err.to_string().contains("min_pass_rate"),
            "error should mention the offending field: {err}"
        );
    }

    #[test]
    fn run_suite_rejects_negative_min_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = suite_yaml(dir.path(), "min_pass: -0.1\n");
        let err = run_suite(&path).expect_err("negative min_pass must be rejected");
        assert!(
            err.to_string().contains("min_pass"),
            "error should mention the offending field: {err}"
        );
    }
}
