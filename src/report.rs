//! The machine-readable CI report (`--report-json`): one versioned JSON document per `eval run`
//! or `eval compare`, for a pipeline to read instead of parsing the printed table.
//!
//! `schema` is `evald.report/v1`, and [`SCHEMA`] (docs/schemas/report-v1.schema.json) is its
//! JSON Schema. Within v1, fields are only added, so a consumer ignores fields it does not know;
//! a field renamed, removed or retyped is v2. A command that could not run still writes a
//! report, `{"passed": false, "error": …}`, so a pipeline always has one to read.

use std::path::Path;

use serde_json::{json, Value};

use crate::eval::{CompareOpts, CompareReport, RunReport};

/// The `schema` every report carries.
pub const VERSION: &str = "evald.report/v1";

/// The JSON Schema a v1 report validates against.
pub const SCHEMA: &str = include_str!("../docs/schemas/report-v1.schema.json");

fn header(kind: &str, passed: bool) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("schema".into(), VERSION.into());
    m.insert("kind".into(), kind.into());
    m.insert("evald_version".into(), env!("CARGO_PKG_VERSION").into());
    m.insert("passed".into(), passed.into());
    m
}

/// The report of an `eval run`.
pub fn run(report: &RunReport) -> Value {
    let mut m = header("eval_run", report.passed());
    m.insert("run_id".into(), report.run_id.clone().into());
    m.insert("name".into(), json!(report.name));
    m.insert("items".into(), report.item_count.into());
    m.insert(
        "evaluators".into(),
        report
            .aggregates
            .iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "mean": a.mean,
                    "pass_rate": a.pass_rate,
                    "scored": a.scored,
                    "skipped": a.skipped,
                    "threshold": a.threshold,
                    "threshold_met": a.threshold_met,
                })
            })
            .collect(),
    );
    m.insert(
        "slices".into(),
        report
            .slices
            .iter()
            .map(|s| {
                json!({
                    "key": s.key,
                    "value": s.value,
                    "evaluator": s.evaluator,
                    "items": s.n,
                    "mean": s.mean,
                })
            })
            .collect(),
    );
    Value::Object(m)
}

/// The report of an `eval compare`, with the gates it applied and whether they passed.
pub fn compare(report: &CompareReport, opts: &CompareOpts, passed: bool) -> Value {
    let mut m = header("eval_compare", passed);
    m.insert("run_a".into(), report.run_a.clone().into());
    m.insert("run_b".into(), report.run_b.clone().into());
    m.insert("alpha".into(), opts.alpha.into());
    m.insert(
        "gates".into(),
        json!({
            "fail_on_regression": opts.fail_on_regression,
            "tolerance": opts.tolerance,
            "significance": opts.significance,
            "non_inferior_margin": opts.non_inferior_margin,
        }),
    );
    m.insert(
        "evaluators".into(),
        report
            .rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.evaluator,
                    "a": r.a,
                    "b": r.b,
                    "delta": r.delta,
                    "n_a": r.n_a,
                    "n_b": r.n_b,
                    "test": r.test.as_ref().map(crate::eval::TestUsed::name),
                    "p_value": r.signif.map(|s| s.p_two_sided),
                    "ci_low": r.signif.map(|s| s.ci_low),
                    "ci_high": r.signif.map(|s| s.ci_high),
                    "bootstrap_ci": r.bootstrap_ci.map(|(lo, hi)| [lo, hi]),
                    "regressed": r.is_regression(opts.tolerance),
                })
            })
            .collect(),
    );
    m.insert("judge_changes".into(), json!(report.judge_changes));
    m.insert(
        "non_inferiority".into(),
        report
            .non_inferiority
            .iter()
            .map(crate::eval::NonInferiorRow::to_json)
            .collect(),
    );
    m.insert(
        "replay".into(),
        json!({ "a": report.replay_a, "b": report.replay_b }),
    );
    Value::Object(m)
}

/// The report of a command that could not run.
pub fn error(kind: &str, message: &str) -> Value {
    let mut m = header(kind, false);
    m.insert("error".into(), message.into());
    Value::Object(m)
}

/// Write the report for `result` to `path` (nothing when `None`): built by `build` on success,
/// the error report otherwise. A report that cannot be written fails the command, unless the
/// command had already failed (then its own error is the one worth seeing).
pub fn write_after<T>(
    path: Option<&Path>,
    result: &anyhow::Result<T>,
    build: impl FnOnce(&T) -> Value,
    kind: &str,
) -> anyhow::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let doc = match result {
        Ok(v) => build(v),
        Err(e) => error(kind, &format!("{e:#}")),
    };
    let text = serde_json::to_string_pretty(&doc).expect("plain data serializes");
    match std::fs::write(path, text + "\n") {
        Ok(()) => Ok(()),
        Err(w) if result.is_err() => {
            eprintln!(
                "evald: could not write --report-json {}: {w}",
                path.display()
            );
            Ok(())
        }
        Err(w) => Err(anyhow::anyhow!(
            "writing --report-json {}: {w}",
            path.display()
        )),
    }
}
