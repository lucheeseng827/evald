//! Cost & token attribution report (`evald cost`).
//!
//! Groups the already-normalized per-span `cost_usd` + token counts by an attribution
//! dimension (model / user / session / service / provider), straight over the `spans` SQL
//! table — no new storage, the data is captured at ingest. Spans that lack the dimension are
//! surfaced as `(untagged)`, so **partial tagging** (the usual attribution failure — you can't
//! govern spend you can't attribute) is visible rather than silently folded away.
//!
//! `cost_usd` is populated only when a span carried an explicit `llm.cost.*` attribute (gen_ai
//! has no cost field; deriving it from a bundled price table is deferred, PLAN.md §2.1), so the
//! token columns are the always-present signal and cost is shown when known.

use std::path::Path;

use crate::{Store, StoreConfig};

/// The attribution dimension to group spend by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Model,
    User,
    Session,
    Service,
    Provider,
}

impl Dimension {
    /// Parse the `--by` value.
    pub fn parse(s: &str) -> anyhow::Result<Dimension> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "model" => Dimension::Model,
            "user" => Dimension::User,
            "session" => Dimension::Session,
            "service" => Dimension::Service,
            "provider" => Dimension::Provider,
            other => anyhow::bail!(
                "unknown --by {other:?} (expected one of: model, user, session, service, provider)"
            ),
        })
    }

    /// The `spans` column this dimension groups on.
    fn column(self) -> &'static str {
        match self {
            Dimension::Model => "model",
            Dimension::User => "user_id",
            Dimension::Session => "session_id",
            Dimension::Service => "service_name",
            Dimension::Provider => "provider",
        }
    }

    /// Human label for the report header.
    fn label(self) -> &'static str {
        match self {
            Dimension::Model => "model",
            Dimension::User => "user",
            Dimension::Session => "session",
            Dimension::Service => "service",
            Dimension::Provider => "provider",
        }
    }
}

/// Run the attribution query and return the rows (one JSON object per attribution group, keyed
/// `attribution` / `spans` / `tokens` / `cost_usd`). Factored out of [`cost_command`] so it is
/// testable without capturing stdout.
pub async fn cost_rows(
    store: &Store,
    dimension: Dimension,
    limit: usize,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let col = dimension.column();
    // Order biggest spend first (NULLs last so a populated row never hides behind an unknown-cost
    // one). `missing_cost` counts the spans in each bucket that carry no `cost_usd`, so a bucket
    // with mixed known/unknown cost can be flagged partial instead of silently understating spend.
    // GROUP BY the RAW column (not COALESCE) so a real value literally named '(untagged)' stays its
    // own group instead of merging into the NULL bucket; the '(untagged)' label is applied in the
    // projection only, for display.
    let sql = format!(
        "SELECT CASE WHEN {col} IS NULL THEN '(untagged)' ELSE {col} END AS attribution, \
                COUNT(*) AS spans, \
                SUM(total_tokens) AS tokens, \
                SUM(cost_usd) AS cost_usd, \
                SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END) AS missing_cost \
         FROM spans \
         GROUP BY {col} \
         ORDER BY cost_usd DESC NULLS LAST, spans DESC"
    );
    Ok(crate::sql::query(store, &sql, limit).await?.rows)
}

/// The TRUE grand totals over **every** span (no `GROUP BY`, no `limit`) — so the footer is a real
/// total, not the sum of only the displayed (possibly limited) buckets. Also returns the total
/// number of attribution groups (to report truncation) and how many spans lack `cost_usd` (to
/// mark the total partial). Returns one JSON row.
async fn grand_total(store: &Store, dimension: Dimension) -> anyhow::Result<serde_json::Value> {
    let col = dimension.column();
    // groups = distinct non-null values + 1 for the NULL/untagged bucket if any exists. Counting
    // DISTINCT COALESCE(...) would fold a real '(untagged)' value into the NULL count and
    // undercount by one.
    let sql = format!(
        "SELECT COUNT(*) AS spans, \
                SUM(total_tokens) AS tokens, \
                SUM(cost_usd) AS cost_usd, \
                SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END) AS missing_cost, \
                COUNT(DISTINCT {col}) \
                  + CASE WHEN SUM(CASE WHEN {col} IS NULL THEN 1 ELSE 0 END) > 0 THEN 1 ELSE 0 END \
                  AS groups \
         FROM spans"
    );
    Ok(crate::sql::query(store, &sql, 1)
        .await?
        .rows
        .into_iter()
        .next()
        .unwrap_or_else(|| serde_json::json!({})))
}

/// `evald cost` — open the store, run the attribution report, print it.
pub async fn cost_command(
    dimension: Dimension,
    data_dir: &Path,
    limit: usize,
) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let rows = cost_rows(&store, dimension, limit).await?;
    let total = grand_total(&store, dimension).await?;
    print_report(dimension, &rows, &total);
    Ok(())
}

fn print_report(dimension: Dimension, rows: &[serde_json::Value], total: &serde_json::Value) {
    println!("\nevald cost — by {}", dimension.label());
    println!(
        "{:<28} {:>10} {:>16} {:>13}",
        dimension.label(),
        "spans",
        "tokens",
        "cost_usd"
    );
    for r in rows {
        let key = r["attribution"].as_str().unwrap_or("?");
        let spans = json_i64(&r["spans"]);
        let tokens = json_i64(&r["tokens"]);
        let missing = json_i64(&r["missing_cost"]);
        // A bucket with SOME known cost but also missing-cost spans understates spend → flag it
        // partial with a trailing `*`. Entirely-unknown buckets just show `-` (clearly unknown).
        let cost_cell = match r["cost_usd"].as_f64() {
            Some(c) if missing > 0 => format!("${c:.4}*"),
            Some(c) => format!("${c:.4}"),
            None => "-".to_string(),
        };
        println!(
            "{:<28} {:>10} {:>16} {:>13}",
            truncate(key, 28),
            spans,
            tokens,
            cost_cell
        );
    }

    // The footer is the TRUE total over every span (not just the displayed buckets).
    let total_spans = json_i64(&total["spans"]);
    let total_tokens = json_i64(&total["tokens"]);
    let total_missing = json_i64(&total["missing_cost"]);
    let total_groups = json_i64(&total["groups"]);
    let total_cost_cell = match total["cost_usd"].as_f64() {
        Some(c) if total_missing > 0 => format!("${c:.4}*"),
        Some(c) => format!("${c:.4}"),
        None => "-".to_string(),
    };
    println!(
        "{:<28} {:>10} {:>16} {:>13}",
        "TOTAL", total_spans, total_tokens, total_cost_cell
    );

    // Truncation: if more attribution groups exist than were shown, say so — the breakdown is
    // partial even though the TOTAL above is complete.
    let shown = rows.len() as i64;
    if total_groups > shown {
        println!(
            "\nnote: showing top {shown} of {total_groups} {} groups (raise --limit to see more); \
             the TOTAL row covers all groups.",
            dimension.label()
        );
    }
    if total_missing > 0 {
        if total["cost_usd"].as_f64().is_some() {
            println!(
                "\nnote: `*` marks a partial cost — {total_missing} span(s) carry no `cost_usd` \
                 (captured only from an `llm.cost.*` attribute), so the marked amounts understate \
                 spend. Token totals are complete."
            );
        } else {
            println!(
                "\nnote: no cost_usd recorded — cost is captured only when a span carries an \
                 `llm.cost.*` attribute. Token totals are always available."
            );
        }
    }
}

/// Read a JSON count as i64 (arrow-json may serialize an aggregate as a number or a string),
/// defaulting to 0 for a SQL NULL (e.g. SUM over all-NULL tokens).
fn json_i64(v: &serde_json::Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

/// Truncate a label to `max` chars (so a long session id doesn't break the table).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;

    fn span_with(
        trace: &str,
        id: &str,
        ts: u64,
        model: Option<&str>,
        tokens: u64,
        cost: Option<f64>,
    ) -> crate::NormalizedSpan {
        let mut s = test_span(trace, id, ts);
        s.model = model.map(String::from);
        s.tokens.total = Some(tokens);
        s.cost_usd = cost;
        s
    }

    #[tokio::test]
    async fn groups_cost_and_tokens_by_model_and_surfaces_untagged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        store
            .append(vec![
                span_with("t", "01", 1, Some("gpt-4o"), 100, Some(0.10)),
                span_with("t", "02", 2, Some("gpt-4o"), 50, Some(0.05)),
                span_with("t", "03", 3, None, 30, None), // untagged, no cost
            ])
            .await
            .unwrap();

        let rows = cost_rows(&store, Dimension::Model, 100).await.unwrap();
        // gpt-4o: 2 spans, 150 tokens, $0.15; (untagged): 1 span, 30 tokens, no cost.
        let gpt = rows
            .iter()
            .find(|r| r["attribution"] == "gpt-4o")
            .expect("gpt-4o row");
        assert_eq!(json_i64(&gpt["spans"]), 2);
        assert_eq!(json_i64(&gpt["tokens"]), 150);
        assert!((gpt["cost_usd"].as_f64().unwrap() - 0.15).abs() < 1e-9);

        let untagged = rows
            .iter()
            .find(|r| r["attribution"] == "(untagged)")
            .expect("untagged row");
        assert_eq!(json_i64(&untagged["spans"]), 1);
        assert_eq!(json_i64(&untagged["tokens"]), 30);
        assert!(untagged["cost_usd"].is_null(), "untagged has no cost");
    }

    #[tokio::test]
    async fn grand_total_is_unbounded_and_flags_partial_cost() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        store
            .append(vec![
                span_with("t", "01", 1, Some("gpt-4o"), 100, Some(0.10)),
                span_with("t", "02", 2, Some("gpt-4o"), 50, None), // gpt-4o: known + unknown → partial
                span_with("t", "03", 3, Some("claude"), 20, Some(0.02)),
                span_with("t", "04", 4, None, 30, None), // untagged, no cost
            ])
            .await
            .unwrap();

        // The breakdown honors --limit (only the top bucket), but the grand total must NOT —
        // it covers every span/group so the footer isn't a misleading subtotal.
        let shown = cost_rows(&store, Dimension::Model, 1).await.unwrap();
        assert_eq!(shown.len(), 1, "limit applies to the displayed breakdown");

        let total = grand_total(&store, Dimension::Model).await.unwrap();
        assert_eq!(
            json_i64(&total["spans"]),
            4,
            "total covers all spans, not the shown bucket"
        );
        assert_eq!(json_i64(&total["tokens"]), 200);
        assert_eq!(json_i64(&total["groups"]), 3, "gpt-4o, claude, (untagged)");
        assert_eq!(
            json_i64(&total["missing_cost"]),
            2,
            "span 02 + span 04 lack cost"
        );
        assert!((total["cost_usd"].as_f64().unwrap() - 0.12).abs() < 1e-9); // 0.10 + 0.02 only

        // The gpt-4o bucket is itself partial: 1 of its 2 spans has no cost.
        let full = cost_rows(&store, Dimension::Model, 100).await.unwrap();
        let gpt = full.iter().find(|r| r["attribution"] == "gpt-4o").unwrap();
        assert_eq!(json_i64(&gpt["spans"]), 2);
        assert_eq!(json_i64(&gpt["missing_cost"]), 1);
    }

    #[tokio::test]
    async fn a_real_untagged_value_does_not_merge_with_null() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        store
            .append(vec![
                span_with("t", "01", 1, None, 10, Some(0.01)), // NULL model → untagged bucket
                span_with("t", "02", 2, Some("(untagged)"), 20, Some(0.02)), // a model literally named so
                span_with("t", "03", 3, Some("gpt-4o"), 30, Some(0.03)),
            ])
            .await
            .unwrap();

        // The literal '(untagged)' model and the NULL model are DISTINCT groups — not merged.
        let total = grand_total(&store, Dimension::Model).await.unwrap();
        assert_eq!(
            json_i64(&total["groups"]),
            3,
            "NULL, '(untagged)', gpt-4o are 3 groups"
        );

        // GROUP BY the raw column keeps them separate (two rows display '(untagged)' but their
        // span/token counts are not collapsed into one).
        let rows = cost_rows(&store, Dimension::Model, 100).await.unwrap();
        assert_eq!(rows.len(), 3, "three distinct groups, not two");
        let untagged_spans: i64 = rows
            .iter()
            .filter(|r| r["attribution"] == "(untagged)")
            .map(|r| json_i64(&r["spans"]))
            .sum();
        assert_eq!(
            untagged_spans, 2,
            "1 NULL + 1 literal, each its own group's span"
        );
    }

    #[test]
    fn dimension_parse_rejects_unknown() {
        assert_eq!(Dimension::parse("model").unwrap(), Dimension::Model);
        assert_eq!(Dimension::parse("USER").unwrap(), Dimension::User);
        assert!(Dimension::parse("team").is_err());
    }
}
