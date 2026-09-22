//! Cost & token attribution report (`evald cost`).
//!
//! Groups the already-normalized per-span `cost_usd` + token counts by an attribution
//! dimension (model / user / session / service / provider), straight over the `spans` SQL
//! table — no new storage, the data is captured at ingest. Spans that lack the dimension are
//! surfaced as `(untagged)`, so **partial tagging** (the usual attribution failure — you can't
//! govern spend you can't attribute) is visible rather than silently folded away.
//!
//! `cost_usd` is the span's own `llm.cost.*` when it carried one, and otherwise the cost
//! [`crate::price`] derived at ingest from the span's model and token counts. Nothing marks a
//! derived cost per span: a reported cost always comes with the attribute it was read from
//! ([`crate::price::COST_ATTRIBUTES`], kept in `raw_attributes`), so a span with a cost and no such
//! attribute was priced by evald. A span whose model is not in the price table has no cost and is
//! shown as `(no price)`, never `$0`. `evald cost --price-table <file>` **re-prices in the query**
//! from the stored token counts (only spans whose cost was derived or is missing; a cost the
//! instrumentor reported is kept) and rewrites nothing.

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

/// Rows fetched when re-pricing: one per distinct (attribution, model, token counts, flags), so
/// this bounds the *distinct* usage shapes, not the span count.
const REPRICE_MAX_GROUPS: usize = 1_000_000;

/// `evald cost` — open the store, run the attribution report, print it. With `price_table`, the
/// costs are re-priced from the stored token counts under that table (layered over the baseline).
pub async fn cost_command(
    dimension: Dimension,
    data_dir: &Path,
    limit: usize,
    price_table: Option<&Path>,
) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let (rows, total) = match price_table {
        None => (
            cost_rows(&store, dimension, limit).await?,
            grand_total(&store, dimension).await?,
        ),
        Some(path) => {
            let table = crate::price::PriceTable::load_over_baseline(path)?;
            crate::price::warn_if_stale(&table);
            let out = reprice(&store, dimension, limit, &table).await?;
            println!(
                "\nre-priced under table {} ({} models); stored spans are unchanged",
                table.version,
                table.len()
            );
            out
        }
    };
    print_report(dimension, &rows, &total);
    Ok(())
}

/// The attribution report with every derivable cost recomputed under `table`, from the stored
/// token counts. A span whose cost the instrumentor reported keeps that cost; a span whose cost
/// evald derived at ingest (or that has none) is priced again. Returns the same row shape as
/// [`cost_rows`] plus the grand total, so [`print_report`] renders either.
pub async fn reprice(
    store: &Store,
    dimension: Dimension,
    limit: usize,
    table: &crate::price::PriceTable,
) -> anyhow::Result<(Vec<serde_json::Value>, serde_json::Value)> {
    use crate::model::Tokens;
    use crate::price::{detect_basis, price_tokens, Basis, COST_ATTRIBUTES};

    let col = dimension.column();
    // A derived cost is one evald filled in: the span has a cost and none of the attributes a
    // reported cost is read from (the normalizer keeps every attribute, so a reported cost always
    // carries its key). Nothing per span says "derived" — that is the point of not stamping it.
    let not_reported = COST_ATTRIBUTES
        .iter()
        .map(|k| format!(r#"raw_attributes_json NOT LIKE '%"{k}":%'"#))
        .collect::<Vec<_>>()
        .join(" AND ");
    // Group by the usage shape so identical requests collapse to one row: the number of rows
    // is the number of DISTINCT (model, token counts), not the number of spans.
    let sql = format!(
        r#"SELECT attribution, model, prompt_tokens, completion_tokens, total_tokens,
                  cache_read_tokens, cache_write_tokens, reasoning_tokens, cost_usd,
                  derived, cache_excl, reasoning_add, COUNT(*) AS n
           FROM (SELECT {col} AS attribution, model, prompt_tokens, completion_tokens,
                        total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        cost_usd,
                        CASE WHEN cost_usd IS NOT NULL AND {not_reported}
                             THEN 1 ELSE 0 END AS derived,
                        CASE WHEN raw_attributes_json LIKE '%"evald.cost.basis":"%cache_excl%'
                             THEN 1 ELSE 0 END AS cache_excl,
                        CASE WHEN raw_attributes_json LIKE '%"evald.cost.basis":"%reasoning_add%'
                             THEN 1 ELSE 0 END AS reasoning_add
                 FROM spans)
           GROUP BY 1,2,3,4,5,6,7,8,9,10,11,12"#
    );
    let resp = crate::sql::query(store, &sql, REPRICE_MAX_GROUPS).await?;
    if resp.truncated {
        anyhow::bail!("more than {REPRICE_MAX_GROUPS} distinct usage shapes; cannot re-price");
    }

    #[derive(Default)]
    struct Acc {
        spans: i64,
        tokens: i64,
        cost: Option<f64>,
        missing: i64,
    }
    let mut groups: std::collections::BTreeMap<String, Acc> = Default::default();
    let mut all = Acc::default();
    for r in &resp.rows {
        let n = json_i64(&r["n"]);
        let key = r["attribution"]
            .as_str()
            .unwrap_or("(untagged)")
            .to_string();
        let opt = |k: &str| r[k].as_u64().or_else(|| r[k].as_i64().map(|v| v as u64));
        let tokens = Tokens {
            prompt: opt("prompt_tokens"),
            completion: opt("completion_tokens"),
            total: opt("total_tokens"),
            cache_read: opt("cache_read_tokens"),
            cache_write: opt("cache_write_tokens"),
            reasoning: opt("reasoning_tokens"),
        };
        let derived = json_i64(&r["derived"]) == 1;
        let stored = r["cost_usd"].as_f64();
        let per_span: Option<f64> = match stored {
            // The instrumentor's own number: kept.
            Some(c) if !derived => Some(c),
            // Derived at ingest, or absent: price it again under this table.
            _ => r["model"].as_str().and_then(|m| {
                if tokens.prompt.is_none() && tokens.completion.is_none() {
                    return None;
                }
                let (_, price) = table.lookup(m)?;
                let basis = if derived {
                    Basis {
                        cache_excl: json_i64(&r["cache_excl"]) == 1,
                        reasoning_add: json_i64(&r["reasoning_add"]) == 1,
                    }
                } else {
                    // Legacy span: only the stored total is left to read the counts by, and the
                    // normalizer synthesises that column as prompt + completion when the
                    // instrumentor reported no total (which `price::reported_total` avoids at
                    // ingest by reading the raw attribute). A synthesized total is not evidence:
                    // it makes `extra` zero, which would silence the "cache larger than the whole
                    // prompt" rule and price a cached span inclusively. Drop it and let the
                    // arithmetic decide.
                    let synthesized = tokens.prompt.zip(tokens.completion).map(|(p, c)| p + c);
                    detect_basis(&tokens, tokens.total.filter(|t| Some(*t) != synthesized))
                };
                Some(price_tokens(price, &tokens, basis))
            }),
        };
        let toks = tokens.total.unwrap_or(0) as i64 * n;
        for a in [groups.entry(key).or_default(), &mut all] {
            a.spans += n;
            a.tokens += toks;
            match per_span {
                Some(c) => *a.cost.get_or_insert(0.0) += c * n as f64,
                None => a.missing += n,
            }
        }
    }

    let row = |k: &str, a: &Acc| {
        serde_json::json!({
            "attribution": k, "spans": a.spans, "tokens": a.tokens,
            "cost_usd": a.cost, "missing_cost": a.missing,
        })
    };
    let mut rows: Vec<serde_json::Value> = groups.iter().map(|(k, a)| row(k, a)).collect();
    rows.sort_by(|a, b| {
        let (ca, cb) = (a["cost_usd"].as_f64(), b["cost_usd"].as_f64());
        // Biggest spend first, unknown cost last, then most spans (the SQL report's order).
        cb.partial_cmp(&ca)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(json_i64(&b["spans"]).cmp(&json_i64(&a["spans"])))
    });
    let mut total = row("TOTAL", &all);
    total["groups"] = serde_json::json!(groups.len());
    rows.truncate(limit);
    Ok((rows, total))
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
            // A named model with no cost is one nothing could price: say so instead of `-`
            // (which reads as "not measured") or `$0` (which would be a lie).
            None if dimension == Dimension::Model && key != "(untagged)" => {
                "(no price)".to_string()
            }
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
                "\nnote: `*` marks a partial cost — {total_missing} span(s) have no cost: the span \
                 reported none and its model is not in the price table (or it has no model or \
                 token counts), so the marked amounts understate spend. Token totals are \
                 complete. Add the model with --price-table <file>."
            );
        } else {
            println!(
                "\nnote: no cost recorded — no span reported one and none could be priced from \
                 the price table (unknown model, or no token counts). Token totals are always \
                 available. Add models with --price-table <file>."
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

    /// A span priced the way ingest would have priced it: a cost, and no cost attribute.
    fn derived_span(
        id: &str,
        model: &str,
        prompt: u64,
        completion: u64,
        cost: f64,
    ) -> crate::NormalizedSpan {
        let mut s = test_span("t", id, 1);
        s.model = Some(model.into());
        s.tokens.prompt = Some(prompt);
        s.tokens.completion = Some(completion);
        s.tokens.total = Some(prompt + completion);
        s.cost_usd = Some(cost);
        s
    }

    #[tokio::test]
    async fn repricing_changes_the_report_and_leaves_the_stored_spans_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        // Two derived spans priced at ingest under an old table ($1 per 1M in, $2 per 1M out) …
        let mut reported = span_with("t", "03", 3, Some("gpt-4o"), 100, Some(5.0)); // … one the app priced itself,
        reported.tokens.prompt = Some(50);
        reported.tokens.completion = Some(50);
        // The attribute the normalizer read the cost from: what marks the cost as reported.
        reported
            .raw_attributes
            .insert("llm.cost.total".into(), 5.0.into());
        let mut unknown = span_with("t", "04", 4, Some("mystery"), 40, None); // … and one nobody could price.
        unknown.tokens.prompt = Some(30);
        unknown.tokens.completion = Some(10);
        store
            .append(vec![
                derived_span("01", "gpt-4o", 1_000_000, 1_000_000, 3.0),
                derived_span("02", "gpt-4o", 1_000_000, 1_000_000, 3.0),
                reported,
                unknown,
            ])
            .await
            .unwrap();
        let stored_before = cost_rows(&store, Dimension::Model, 100).await.unwrap();

        // The corrected table: $10 in / $20 out per 1M. Each derived span is now 10 + 20 = $30.
        let table = crate::price::PriceTable::from_json(
            r#"{"gpt-4o": {"input_cost_per_token": 1e-5, "output_cost_per_token": 2e-5}}"#,
        )
        .unwrap();
        let (rows, total) = reprice(&store, Dimension::Model, 100, &table)
            .await
            .unwrap();
        let gpt = rows.iter().find(|r| r["attribution"] == "gpt-4o").unwrap();
        assert_eq!(json_i64(&gpt["spans"]), 3);
        // 2 re-priced derived spans ($30 each) + the reported $5 kept as-is.
        assert!(
            (gpt["cost_usd"].as_f64().unwrap() - 65.0).abs() < 1e-6,
            "{gpt}"
        );
        // The stored report (no re-pricing) is exactly what it was: nothing was rewritten.
        let stored_after = cost_rows(&store, Dimension::Model, 100).await.unwrap();
        assert_eq!(stored_before, stored_after);
        let stored_gpt = stored_after
            .iter()
            .find(|r| r["attribution"] == "gpt-4o")
            .unwrap();
        assert!((stored_gpt["cost_usd"].as_f64().unwrap() - 11.0).abs() < 1e-6); // 3 + 3 + 5

        // The unpriceable model stays unpriced (missing), not $0, and marks the total partial.
        let mystery = rows.iter().find(|r| r["attribution"] == "mystery").unwrap();
        assert!(mystery["cost_usd"].is_null());
        assert_eq!(json_i64(&mystery["missing_cost"]), 1);
        assert_eq!(json_i64(&total["missing_cost"]), 1);
        assert_eq!(json_i64(&total["spans"]), 4);
        assert_eq!(json_i64(&total["groups"]), 2);
    }

    #[tokio::test]
    async fn repricing_reads_the_stored_basis_so_the_cache_is_not_counted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        // An input count that EXCLUDES the cache (stamped at ingest): 600 uncached + 400 cached.
        let mut s = derived_span("01", "m", 600, 200, 0.0);
        s.tokens.cache_read = Some(400);
        s.tokens.total = Some(1200);
        s.raw_attributes
            .insert("evald.cost.basis".into(), "cache_excl".into());
        store.append(vec![s]).await.unwrap();
        let table = crate::price::PriceTable::from_json(
            r#"{"m": {"input_cost_per_token": 1e-6, "output_cost_per_token": 4e-6,
                      "cache_read_input_token_cost": 1e-7}}"#,
        )
        .unwrap();
        let (rows, _) = reprice(&store, Dimension::Model, 10, &table).await.unwrap();
        // 600*1e-6 + 400*1e-7 + 200*4e-6 = 0.00144 (the inclusive reading would give 0.00136).
        assert!(
            (rows[0]["cost_usd"].as_f64().unwrap() - 0.00144).abs() < 1e-12,
            "{}",
            rows[0]
        );
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
