//! JUnit XML for the CI gates (`--junit <path>` on `eval run`, `eval compare` and `suite run`).
//!
//! The report is the format GitHub, GitLab and Jenkins render natively as a test report, so a
//! failing gate shows up as a named, expandable failure instead of a wall of log text. One
//! `<testsuite>` per invocation; one `<testcase>` per evaluator (`eval run`), per evaluator delta
//! (`eval compare`) or per suite case plus the suite gate itself (`suite run`).
//!
//! * **Written when the gate fails.** That is when it matters. The exit code is decided by the
//!   caller and is unchanged.
//! * **Written when the command errors** (a missing config, an unknown run id): as a single
//!   `<error>` test case, so the report never silently goes missing.
//! * **Deterministic.** Cases keep the report's own order and nothing depends on the clock except
//!   the suite's `time` (wall-clock seconds for the whole command; individual cases carry
//!   `time="0.000"`, because the evaluators are not timed one by one).
//! * **Always well-formed.** Text that came from a model or a judge can contain anything; every
//!   character XML 1.0 forbids is replaced with U+FFFD and the markup characters are escaped, so
//!   the file parses whatever the data was.
//! * **Counts add up.** `tests = pass + failures + errors + skipped`, always.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

use crate::eval::{CompareReport, RunReport};
use crate::suite::SuiteOutcome;

/// What happened to one test case.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass,
    /// The gate said no. `kind` becomes the `type` attribute (e.g. `ThresholdNotMet`).
    Fail {
        kind: &'static str,
        message: String,
        detail: String,
    },
    /// The command itself could not run.
    Error {
        message: String,
        detail: String,
    },
    /// Nothing to judge (an evaluator that scored no items, an evaluator present on one side).
    Skipped {
        message: String,
    },
}

/// One `<testcase>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub name: String,
    pub outcome: Outcome,
    /// Rendered as `<system-out>`: the numbers behind the verdict.
    pub output: Option<String>,
}

/// One `<testsuite>` (wrapped in a `<testsuites>` root, which some viewers require).
#[derive(Debug, Clone, PartialEq)]
pub struct Suite {
    pub name: String,
    /// Shared `classname` of every case, e.g. `evald.eval.run`.
    pub classname: String,
    pub time_secs: f64,
    pub properties: Vec<(String, String)>,
    pub cases: Vec<Case>,
}

impl Suite {
    fn tally(&self) -> (usize, usize, usize, usize) {
        let mut f = 0;
        let mut e = 0;
        let mut s = 0;
        for c in &self.cases {
            match c.outcome {
                Outcome::Fail { .. } => f += 1,
                Outcome::Error { .. } => e += 1,
                Outcome::Skipped { .. } => s += 1,
                Outcome::Pass => {}
            }
        }
        (self.cases.len(), f, e, s)
    }
}

/// Is `c` allowed in an XML 1.0 document?
fn xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// Escape for element text.
fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if xml_char(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

/// Escape for a double-quoted attribute value (newlines and tabs are kept as references, since
/// a parser normalizes literal ones to a space).
fn attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if xml_char(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

/// Render the report. Pure: the same [`Suite`] always yields the same bytes.
pub fn render(suite: &Suite) -> String {
    let (tests, failures, errors, skipped) = suite.tally();
    let counts = format!(
        "tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{:.3}\"",
        suite.time_secs
    );
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(x, "<testsuites name=\"{}\" {counts}>", attr(&suite.name));
    let _ = writeln!(x, "  <testsuite name=\"{}\" {counts}>", attr(&suite.name));
    if !suite.properties.is_empty() {
        x.push_str("    <properties>\n");
        for (k, v) in &suite.properties {
            let _ = writeln!(
                x,
                "      <property name=\"{}\" value=\"{}\"/>",
                attr(k),
                attr(v)
            );
        }
        x.push_str("    </properties>\n");
    }
    for c in &suite.cases {
        let _ = write!(
            x,
            "    <testcase classname=\"{}\" name=\"{}\" time=\"0.000\"",
            attr(&suite.classname),
            attr(&c.name)
        );
        if matches!(c.outcome, Outcome::Pass) && c.output.is_none() {
            x.push_str("/>\n");
            continue;
        }
        x.push_str(">\n");
        match &c.outcome {
            Outcome::Pass => {}
            Outcome::Fail {
                kind,
                message,
                detail,
            } => {
                let _ = writeln!(
                    x,
                    "      <failure message=\"{}\" type=\"{}\">{}</failure>",
                    attr(message),
                    attr(kind),
                    text(detail)
                );
            }
            Outcome::Error { message, detail } => {
                let _ = writeln!(
                    x,
                    "      <error message=\"{}\" type=\"Error\">{}</error>",
                    attr(message),
                    text(detail)
                );
            }
            Outcome::Skipped { message } => {
                let _ = writeln!(x, "      <skipped message=\"{}\"/>", attr(message));
            }
        }
        if let Some(out) = &c.output {
            let _ = writeln!(x, "      <system-out>{}</system-out>", text(out));
        }
        x.push_str("    </testcase>\n");
    }
    x.push_str("  </testsuite>\n</testsuites>\n");
    x
}

/// Write the report, creating the parent directory if needed.
pub fn write(path: &Path, suite: &Suite) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, render(suite))
}

/// Write the JUnit report for a command that has just finished (or failed), without letting a
/// report problem hide the command's own outcome: if the command already failed, a write error
/// is only reported on stderr; if the command succeeded, the write error is returned (a CI job
/// that asked for a report must not pass without one).
pub fn write_after<T>(
    path: Option<&Path>,
    result: &anyhow::Result<T>,
    build: impl FnOnce(&T) -> Suite,
    error_suite_name: &str,
    classname: &str,
    elapsed_secs: f64,
) -> anyhow::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let suite = match result {
        Ok(v) => {
            let mut s = build(v);
            s.time_secs = elapsed_secs;
            s
        }
        Err(e) => error_suite(error_suite_name, classname, &format!("{e:#}"), elapsed_secs),
    };
    match write(path, &suite) {
        Ok(()) => Ok(()),
        Err(w) if result.is_err() => {
            eprintln!("evald: could not write --junit {}: {w}", path.display());
            Ok(())
        }
        Err(w) => Err(anyhow::anyhow!("writing --junit {}: {w}", path.display())),
    }
}

/// A report for a command that could not run at all: one errored case, so the CI test tab shows
/// why instead of an empty report or none.
pub fn error_suite(name: &str, classname: &str, message: &str, time_secs: f64) -> Suite {
    Suite {
        name: name.to_string(),
        classname: classname.to_string(),
        time_secs,
        properties: vec![],
        cases: vec![Case {
            name: "run".to_string(),
            outcome: Outcome::Error {
                message: first_line(message),
                detail: message.to_string(),
            },
            output: None,
        }],
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// `eval run`: one case per evaluator, judged against its threshold.
pub fn from_run(report: &RunReport) -> Suite {
    let label = report.name.clone().unwrap_or_else(|| report.run_id.clone());
    let cases = report
        .aggregates
        .iter()
        .map(|a| {
            let threshold = a
                .threshold
                .map(|t| format!("{t:.3}"))
                .unwrap_or_else(|| "none".to_string());
            let numbers = format!(
                "mean={:.3} pass_rate={:.3} scored={} skipped={} threshold={threshold}",
                a.mean, a.pass_rate, a.scored, a.skipped
            );
            let outcome = if a.scored == 0 {
                Outcome::Skipped {
                    message: "no items scored (every item was skipped)".to_string(),
                }
            } else if !a.threshold_met {
                Outcome::Fail {
                    kind: "ThresholdNotMet",
                    message: format!(
                        "{}: mean {:.3} is below the threshold {threshold}",
                        a.name, a.mean
                    ),
                    detail: numbers.clone(),
                }
            } else {
                Outcome::Pass
            };
            Case {
                name: a.name.clone(),
                outcome,
                output: Some(numbers),
            }
        })
        .collect();
    Suite {
        name: format!("evald eval run {label}"),
        classname: "evald.eval.run".to_string(),
        time_secs: 0.0,
        properties: vec![
            ("run_id".into(), report.run_id.clone()),
            ("items".into(), report.item_count.to_string()),
        ],
        cases,
    }
}

/// `eval compare`: one case per evaluator delta. A case fails exactly when the gate the command
/// applied (`--fail-on-regression`, with or without `--significance`) fails on that row.
pub fn from_compare(
    report: &CompareReport,
    tolerance: f64,
    significance: bool,
    alpha: f64,
    fail_on_regression: bool,
) -> Suite {
    let cell = |v: Option<f64>| v.map(|x| format!("{x:.3}")).unwrap_or_else(|| "-".into());
    let cases = report
        .rows
        .iter()
        .map(|r| {
            let stat = match r.signif {
                Some(s) => format!(
                    "p={:.4} ci={:.0}%[{:+.3},{:+.3}] {}",
                    s.p_two_sided,
                    (1.0 - alpha) * 100.0,
                    s.ci_low,
                    s.ci_high,
                    if s.ci_high < 0.0 || s.ci_low > 0.0 {
                        "significant"
                    } else {
                        "within noise"
                    }
                ),
                None => "no significance test (missing or too-small sample)".to_string(),
            };
            let numbers = format!(
                "run_a={} run_b={} delta={} n_a={} n_b={} {stat}",
                cell(r.a),
                cell(r.b),
                r.delta
                    .map(|d| format!("{d:+.3}"))
                    .unwrap_or_else(|| "-".into()),
                r.n_a.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
                r.n_b.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
            );
            let regressed = r.is_regression(tolerance);
            let gates = fail_on_regression
                && if significance {
                    r.gates_in_significance_mode(tolerance)
                } else {
                    regressed
                };
            let outcome = match (r.a, r.b) {
                (None, Some(_)) => Outcome::Skipped {
                    message: "evaluator only in run B".into(),
                },
                (Some(_), None) => Outcome::Skipped {
                    message: "evaluator only in run A".into(),
                },
                _ if gates => Outcome::Fail {
                    kind: "Regression",
                    message: format!(
                        "{}: {} -> {} (delta {}) is beyond the tolerance {tolerance:.3}",
                        r.evaluator,
                        cell(r.a),
                        cell(r.b),
                        r.delta.map(|d| format!("{d:+.3}")).unwrap_or_default()
                    ),
                    detail: numbers.clone(),
                },
                _ => Outcome::Pass,
            };
            let note = if regressed && !gates {
                if !fail_on_regression {
                    "; regressed beyond tolerance, not gated (--fail-on-regression is off)"
                } else {
                    "; regressed beyond tolerance but proven within sampling noise"
                }
            } else {
                ""
            };
            Case {
                name: r.evaluator.clone(),
                outcome,
                output: Some(format!("{numbers}{note}")),
            }
        })
        .collect();
    Suite {
        name: format!("evald eval compare {} vs {}", report.run_a, report.run_b),
        classname: "evald.eval.compare".to_string(),
        time_secs: 0.0,
        properties: vec![
            ("run_a".into(), report.run_a.clone()),
            ("run_b".into(), report.run_b.clone()),
            ("tolerance".into(), format!("{tolerance:.3}")),
            ("significance".into(), significance.to_string()),
            ("alpha".into(), format!("{alpha}")),
            ("fail_on_regression".into(), fail_on_regression.to_string()),
        ],
        cases,
    }
}

/// `suite run`: one case per suite case, plus a final case for the suite's own pass-rate gate.
/// A case can fail while the suite still passes (`min_pass_rate` below 1.0 tolerates some);
/// the report shows that truthfully and the gate case carries the verdict the exit code follows.
pub fn from_suite(outcome: &SuiteOutcome, suite_name: &str) -> Suite {
    let mut cases: Vec<Case> = outcome
        .cases
        .iter()
        .map(|c| {
            let numbers = format!("{}/{} run(s) passed", c.runs_passed, c.runs_total);
            Case {
                name: c.name.clone(),
                outcome: if c.passed {
                    Outcome::Pass
                } else {
                    Outcome::Fail {
                        kind: "CaseFailed",
                        message: format!("{}: {numbers}", c.name),
                        detail: numbers.clone(),
                    }
                },
                output: Some(numbers),
            }
        })
        .collect();
    let rate = format!(
        "{:.1}% of cases passed (min_pass_rate {:.1}%)",
        outcome.pass_rate * 100.0,
        outcome.min_pass_rate * 100.0
    );
    cases.push(Case {
        name: "suite pass rate".to_string(),
        outcome: if outcome.passed {
            Outcome::Pass
        } else {
            Outcome::Fail {
                kind: "SuiteGate",
                message: rate.clone(),
                detail: rate.clone(),
            }
        },
        output: Some(rate),
    });
    Suite {
        name: format!("evald suite {suite_name}"),
        classname: "evald.suite".to_string(),
        time_secs: 0.0,
        properties: vec![],
        cases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{CompareRow, EvaluatorAggregate};
    use crate::stats::WelchResult;
    use crate::suite::CaseResult;

    fn agg(
        name: &str,
        mean: f64,
        scored: usize,
        threshold: Option<f64>,
        met: bool,
    ) -> EvaluatorAggregate {
        EvaluatorAggregate {
            name: name.into(),
            mean,
            pass_rate: mean,
            scored,
            skipped: 0,
            threshold,
            threshold_met: met,
        }
    }

    fn run(aggs: Vec<EvaluatorAggregate>) -> RunReport {
        RunReport {
            run_id: "run-1".into(),
            name: Some("nightly".into()),
            item_count: 10,
            aggregates: aggs,
        }
    }

    /// A minimal, dependency-free well-formedness check: every tag closes, in order, and no
    /// character XML 1.0 forbids survives. (There is no XML crate in the tree, and a real parser
    /// is not worth adding for a test.)
    fn assert_well_formed(xml: &str) {
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(
            xml.chars().all(xml_char),
            "invalid XML 1.0 character survived"
        );
        let body = &xml[xml.find("?>").unwrap() + 2..];
        let mut stack: Vec<String> = vec![];
        let mut rest = body;
        while let Some(open) = rest.find('<') {
            let after = &rest[open + 1..];
            let close = after.find('>').expect("unterminated tag");
            let tag = &after[..close];
            assert!(!tag.contains('<'), "'<' inside a tag: {tag}");
            if let Some(name) = tag.strip_prefix('/') {
                assert_eq!(
                    stack.pop().as_deref(),
                    Some(name),
                    "mismatched close </{name}>"
                );
            } else if !tag.ends_with('/') {
                stack.push(tag.split_whitespace().next().unwrap().to_string());
            }
            // Text between tags must not contain a bare '&' that is not an entity.
            let text_run = &after[close + 1
                ..after[close + 1..]
                    .find('<')
                    .map_or(after.len(), |n| close + 1 + n)];
            for (i, _) in text_run.match_indices('&') {
                let e = &text_run[i..];
                assert!(
                    ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;", "&#10;", "&#13;", "&#9;"]
                        .iter()
                        .any(|ent| e.starts_with(ent)),
                    "bare '&' in text: {e:.20}"
                );
            }
            rest = &after[close + 1..];
        }
        assert!(stack.is_empty(), "unclosed tags: {stack:?}");
    }

    fn counts(xml: &str) -> (usize, usize, usize, usize) {
        let get = |k: &str| {
            let i = xml.find(&format!("{k}=\"")).unwrap() + k.len() + 2;
            xml[i..i + xml[i..].find('"').unwrap()]
                .parse::<usize>()
                .unwrap()
        };
        (get("tests"), get("failures"), get("errors"), get("skipped"))
    }

    // --- golden files: the exact bytes CI viewers will see -----------------------------------

    #[test]
    fn eval_run_pass_golden() {
        let mut s = from_run(&run(vec![
            agg("exact_match", 0.9, 10, Some(0.8), true),
            agg("non_empty", 1.0, 10, None, true),
        ]));
        s.time_secs = 1.5;
        let expected = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<testsuites name=\"evald eval run nightly\" tests=\"2\" failures=\"0\" errors=\"0\" skipped=\"0\" time=\"1.500\">
  <testsuite name=\"evald eval run nightly\" tests=\"2\" failures=\"0\" errors=\"0\" skipped=\"0\" time=\"1.500\">
    <properties>
      <property name=\"run_id\" value=\"run-1\"/>
      <property name=\"items\" value=\"10\"/>
    </properties>
    <testcase classname=\"evald.eval.run\" name=\"exact_match\" time=\"0.000\">
      <system-out>mean=0.900 pass_rate=0.900 scored=10 skipped=0 threshold=0.800</system-out>
    </testcase>
    <testcase classname=\"evald.eval.run\" name=\"non_empty\" time=\"0.000\">
      <system-out>mean=1.000 pass_rate=1.000 scored=10 skipped=0 threshold=none</system-out>
    </testcase>
  </testsuite>
</testsuites>
";
        assert_eq!(render(&s), expected);
        assert_well_formed(&render(&s));
    }

    #[test]
    fn eval_run_fail_golden_and_counts() {
        let s = from_run(&run(vec![
            agg("exact_match", 0.7, 10, Some(0.8), false),
            agg("regex", 0.0, 0, Some(0.5), true), // scored nothing: skipped, never a failure
            agg("non_empty", 1.0, 10, None, true),
        ]));
        let xml = render(&s);
        assert!(xml.contains(
            "<failure message=\"exact_match: mean 0.700 is below the threshold 0.800\" type=\"ThresholdNotMet\">mean=0.700 pass_rate=0.700 scored=10 skipped=0 threshold=0.800</failure>"
        ), "{xml}");
        assert!(xml.contains("<skipped message=\"no items scored (every item was skipped)\"/>"));
        assert_eq!(counts(&xml), (3, 1, 0, 1));
        assert_well_formed(&xml);
    }

    fn welch(p: f64, lo: f64, hi: f64) -> WelchResult {
        WelchResult {
            t: 0.0,
            df: 10.0,
            p_two_sided: p,
            ci_low: lo,
            ci_high: hi,
        }
    }

    fn row(name: &str, a: Option<f64>, b: Option<f64>, signif: Option<WelchResult>) -> CompareRow {
        CompareRow {
            evaluator: name.into(),
            a,
            b,
            delta: a.zip(b).map(|(a, b)| b - a),
            n_a: a.map(|_| 100),
            n_b: b.map(|_| 100),
            signif,
        }
    }

    fn compare(rows: Vec<CompareRow>) -> CompareReport {
        CompareReport {
            run_a: "good".into(),
            run_b: "bad".into(),
            rows,
        }
    }

    #[test]
    fn compare_regression_golden() {
        let rep = compare(vec![row(
            "exact_match",
            Some(0.9),
            Some(0.7),
            Some(welch(0.0123, -0.3, -0.1)),
        )]);
        let xml = render(&from_compare(&rep, 0.0, true, 0.05, true));
        assert!(xml.contains(
            "<failure message=\"exact_match: 0.900 -&gt; 0.700 (delta -0.200) is beyond the tolerance 0.000\" type=\"Regression\">run_a=0.900 run_b=0.700 delta=-0.200 n_a=100 n_b=100 p=0.0123 ci=95%[-0.300,-0.100] significant</failure>"
        ), "{xml}");
        assert!(xml.contains("<property name=\"significance\" value=\"true\"/>"));
        assert_eq!(counts(&xml), (1, 1, 0, 0));
        assert_well_formed(&xml);
    }

    #[test]
    fn compare_case_fails_exactly_when_the_gate_does() {
        let rep = compare(vec![
            row(
                "drop_sig",
                Some(0.9),
                Some(0.7),
                Some(welch(0.01, -0.3, -0.1)),
            ),
            row(
                "drop_noise",
                Some(0.9),
                Some(0.85),
                Some(welch(0.4, -0.2, 0.1)),
            ),
            row("same", Some(0.8), Some(0.8), None),
            row("only_b", None, Some(0.5), None),
            row("only_a", Some(0.5), None, None),
        ]);
        // Significance mode: the noisy drop is forgiven, the significant one gates.
        let xml = render(&from_compare(&rep, 0.0, true, 0.05, true));
        assert_eq!(counts(&xml), (5, 1, 0, 2));
        assert!(xml.contains("proven within sampling noise"));
        // Raw-delta mode: both drops gate.
        let xml = render(&from_compare(&rep, 0.0, false, 0.05, true));
        assert_eq!(counts(&xml), (5, 2, 0, 2));
        // No --fail-on-regression: nothing fails, and the note says why.
        let xml = render(&from_compare(&rep, 0.0, false, 0.05, false));
        assert_eq!(counts(&xml), (5, 0, 0, 2));
        assert!(xml.contains("not gated (--fail-on-regression is off)"));
        assert_well_formed(&xml);
    }

    fn case_result(name: &str, passed: usize, total: usize, ok: bool) -> CaseResult {
        CaseResult {
            name: name.into(),
            runs_total: total,
            runs_passed: passed,
            passed: ok,
        }
    }

    #[test]
    fn suite_golden_with_a_tolerated_failure() {
        // One of two cases failed, but min_pass_rate 0.5 lets the suite pass: the report keeps
        // the case failure and the gate case (which the exit code follows) passes.
        let out = crate::suite::aggregate(
            vec![case_result("a", 1, 1, true), case_result("b", 0, 1, false)],
            0.5,
        );
        let xml = render(&from_suite(&out, "smoke.yaml"));
        assert!(xml.contains("<failure message=\"b: 0/1 run(s) passed\" type=\"CaseFailed\">"));
        assert!(xml.contains(
            "<testcase classname=\"evald.suite\" name=\"suite pass rate\" time=\"0.000\">\n      <system-out>50.0% of cases passed (min_pass_rate 50.0%)</system-out>"
        ));
        assert_eq!(counts(&xml), (3, 1, 0, 0));
        assert_well_formed(&xml);
    }

    #[test]
    fn a_failing_suite_gate_is_a_failure() {
        let out = crate::suite::aggregate(vec![case_result("a", 0, 1, false)], 1.0);
        let xml = render(&from_suite(&out, "s.yaml"));
        assert!(xml.contains("type=\"SuiteGate\""));
        assert_eq!(counts(&xml), (2, 2, 0, 0));
    }

    #[test]
    fn empty_suite_is_just_the_vacuously_passing_gate() {
        let out = crate::suite::aggregate(vec![], 1.0);
        let xml = render(&from_suite(&out, "empty.yaml"));
        assert_eq!(counts(&xml), (1, 0, 0, 0));
        assert_well_formed(&xml);
        // An eval run with no evaluators is a well-formed empty testsuite too.
        let xml = render(&from_run(&run(vec![])));
        assert_eq!(counts(&xml), (0, 0, 0, 0));
        assert_well_formed(&xml);
    }

    #[test]
    fn escaping_survives_hostile_judge_and_model_output() {
        // Markup, quotes, an ampersand entity lookalike, a newline, a NUL, a form feed, an
        // unpaired-surrogate stand-in (U+FFFF) and an emoji (valid, must survive).
        let nasty = "a<b>&amp; \"q\" 'x'\nline2\u{0}\u{c}\u{FFFF} ]]> 😀";
        let mut r = run(vec![agg(nasty, 0.1, 1, Some(0.5), false)]);
        r.name = Some(nasty.into());
        let xml = render(&from_run(&r));
        assert_well_formed(&xml);
        assert!(xml.contains("&lt;b&gt;&amp;amp;"), "{xml}");
        assert!(xml.contains("&quot;q&quot;") && xml.contains("&apos;x&apos;"));
        assert!(
            xml.contains("&#10;"),
            "a newline in an attribute must be a reference"
        );
        assert!(xml.contains('😀'));
        assert!(!xml.contains('\u{0}') && !xml.contains('\u{c}') && !xml.contains('\u{FFFF}'));
        assert!(
            xml.contains('\u{FFFD}'),
            "invalid characters are replaced, not dropped silently"
        );
        assert_eq!(counts(&xml), (1, 1, 0, 0));
    }

    #[test]
    fn a_command_that_could_not_run_becomes_one_errored_case() {
        let s = error_suite(
            "evald eval run",
            "evald.eval.run",
            "reading config x: not found\nmore",
            0.25,
        );
        let xml = render(&s);
        assert!(xml.contains("<error message=\"reading config x: not found\" type=\"Error\">reading config x: not found\nmore</error>"));
        assert_eq!(counts(&xml), (1, 0, 1, 0));
        assert_well_formed(&xml);
    }

    #[test]
    fn write_after_reports_even_when_the_command_failed_and_keeps_the_original_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/out.xml"); // parent is created
        let failed: anyhow::Result<RunReport> = Err(anyhow::anyhow!("boom"));
        write_after(
            Some(&path),
            &failed,
            from_run,
            "evald eval run",
            "evald.eval.run",
            0.1,
        )
        .unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(xml.contains("boom"));
        // A report that cannot be written after a SUCCESSFUL command is an error; after a failed
        // one it must not mask the failure.
        let bad = dir.path().join("f"); // a file where a directory is needed
        std::fs::write(&bad, "x").unwrap();
        let unwritable = bad.join("out.xml");
        let ok: anyhow::Result<RunReport> = Ok(run(vec![]));
        assert!(write_after(Some(&unwritable), &ok, from_run, "n", "c", 0.0).is_err());
        assert!(write_after(Some(&unwritable), &failed, from_run, "n", "c", 0.0).is_ok());
        // No path: nothing to do.
        assert!(write_after(None, &ok, from_run, "n", "c", 0.0).is_ok());
    }
}
