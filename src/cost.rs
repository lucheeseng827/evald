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

/// One row per distinct usage shape (attribution, model, token counts, cost, flags) among the
/// spans `filter` selects, with `n` the spans of that shape: re-pricing reads these, so the work
/// is bounded by the distinct shapes, not the span count.
async fn usage_shapes(
    store: &Store,
    col: &str,
    filter: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    use crate::price::COST_ATTRIBUTES;
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
                 FROM spans WHERE {filter})
           GROUP BY 1,2,3,4,5,6,7,8,9,10,11,12"#
    );
    let resp = crate::sql::query(store, &sql, REPRICE_MAX_GROUPS).await?;
    if resp.truncated {
        anyhow::bail!("more than {REPRICE_MAX_GROUPS} distinct usage shapes; cannot re-price");
    }
    Ok(resp.rows)
}

/// The token counts of a usage-shape row.
fn shape_tokens(r: &serde_json::Value) -> crate::model::Tokens {
    let opt = |k: &str| r[k].as_u64().or_else(|| r[k].as_i64().map(|v| v as u64));
    crate::model::Tokens {
        prompt: opt("prompt_tokens"),
        completion: opt("completion_tokens"),
        total: opt("total_tokens"),
        cache_read: opt("cache_read_tokens"),
        cache_write: opt("cache_write_tokens"),
        reasoning: opt("reasoning_tokens"),
    }
}

/// How a shape's counts are to be read: the basis evald recorded when it priced the span at
/// ingest, else worked out from the counts.
fn shape_basis(
    r: &serde_json::Value,
    tokens: &crate::model::Tokens,
    derived: bool,
) -> crate::price::Basis {
    use crate::price::{detect_basis, Basis};
    if derived {
        return Basis {
            cache_excl: json_i64(&r["cache_excl"]) == 1,
            reasoning_add: json_i64(&r["reasoning_add"]) == 1,
        };
    }
    // Legacy span: only the stored total is left to read the counts by, and the normalizer
    // synthesises that column as prompt + completion when the instrumentor reported no total
    // (which `price::reported_total` avoids at ingest by reading the raw attribute). A
    // synthesized total is not evidence: it makes `extra` zero, which would silence the "cache
    // larger than the whole prompt" rule and price a cached span inclusively. Drop it and let
    // the arithmetic decide.
    let synthesized = tokens.prompt.zip(tokens.completion).map(|(p, c)| p + c);
    detect_basis(tokens, tokens.total.filter(|t| Some(*t) != synthesized))
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
    use crate::price::price_tokens;

    #[derive(Default)]
    struct Acc {
        spans: i64,
        tokens: i64,
        cost: Option<f64>,
        missing: i64,
    }
    let mut groups: std::collections::BTreeMap<String, Acc> = Default::default();
    let mut all = Acc::default();
    for r in &usage_shapes(store, dimension.column(), "TRUE").await? {
        let n = json_i64(&r["n"]);
        let key = r["attribution"]
            .as_str()
            .unwrap_or("(untagged)")
            .to_string();
        let tokens = shape_tokens(r);
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
                Some(price_tokens(
                    price,
                    &tokens,
                    shape_basis(r, &tokens, derived),
                ))
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

/// `models` as a SQL `IN (…)` list, quoted.
fn sql_in(models: &[String]) -> String {
    models
        .iter()
        .map(|m| format!("'{}'", m.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One row of [`reprice_as`]: a group's model calls, what they cost as stored, and what the same
/// tokens cost at the hosted model's list price.
#[derive(Debug, Clone, PartialEq)]
pub struct AsHostedRow {
    pub attribution: String,
    pub calls: i64,
    pub tokens: i64,
    /// The stored cost, where the calls had one (`None`: none did).
    pub cost_usd: Option<f64>,
    /// Calls with no stored cost.
    pub missing_cost: i64,
    pub as_hosted_usd: f64,
    /// Calls with no token counts, so they could not be re-priced.
    pub no_tokens: i64,
}

/// What this traffic would have cost at `hosted`'s list price: every model call (of `models`,
/// when given) priced from its own token counts under the hosted model's rates, beside the cost
/// stored for it. Groups by `dimension`; the last row is the total.
pub async fn reprice_as(
    store: &Store,
    dimension: Dimension,
    limit: usize,
    table: &crate::price::PriceTable,
    hosted: &str,
    models: &[String],
) -> anyhow::Result<Vec<AsHostedRow>> {
    use crate::price::price_tokens;
    let (_, price) = table.lookup(hosted).ok_or_else(|| {
        anyhow::anyhow!(
            "{hosted:?} is not in the price table — name a model it prices (`evald cost --by \
             model` shows what is priced), or add it with --price-table"
        )
    })?;
    let mut filter = crate::latency::LLM_PREDICATE.to_string();
    if !models.is_empty() {
        filter = format!("{filter} AND model IN ({})", sql_in(models));
    }
    let mut groups: std::collections::BTreeMap<String, AsHostedRow> = Default::default();
    let blank = |k: &str| AsHostedRow {
        attribution: k.to_string(),
        calls: 0,
        tokens: 0,
        cost_usd: None,
        missing_cost: 0,
        as_hosted_usd: 0.0,
        no_tokens: 0,
    };
    let mut total = blank("TOTAL");
    for r in &usage_shapes(store, dimension.column(), &filter).await? {
        let n = json_i64(&r["n"]);
        let key = r["attribution"]
            .as_str()
            .unwrap_or("(untagged)")
            .to_string();
        let tokens = shape_tokens(r);
        let derived = json_i64(&r["derived"]) == 1;
        let priced = (tokens.prompt.is_some() || tokens.completion.is_some())
            .then(|| price_tokens(price, &tokens, shape_basis(r, &tokens, derived)));
        let entry = groups.entry(key.clone()).or_insert_with(|| blank(&key));
        for a in [entry, &mut total] {
            a.calls += n;
            a.tokens += tokens.total.unwrap_or(0) as i64 * n;
            match r["cost_usd"].as_f64() {
                Some(c) => *a.cost_usd.get_or_insert(0.0) += c * n as f64,
                None => a.missing_cost += n,
            }
            match priced {
                Some(c) => a.as_hosted_usd += c * n as f64,
                None => a.no_tokens += n,
            }
        }
    }
    let mut rows: Vec<AsHostedRow> = groups.into_values().collect();
    rows.sort_by(|a, b| {
        b.as_hosted_usd
            .total_cmp(&a.as_hosted_usd)
            .then(b.calls.cmp(&a.calls))
    });
    rows.truncate(limit);
    rows.push(total);
    Ok(rows)
}

fn print_as_hosted(dimension: Dimension, hosted: &str, price: (f64, f64), rows: &[AsHostedRow]) {
    let col = format!("as {}", truncate(hosted, 20));
    println!(
        "\nevald cost — model calls by {}, priced as {hosted} (${:.2} / ${:.2} per 1M input / output)",
        dimension.label(),
        price.0 * 1e6,
        price.1 * 1e6
    );
    println!(
        "{:<28} {:>10} {:>16} {:>13} {:>16}",
        dimension.label(),
        "calls",
        "tokens",
        "cost_usd",
        col
    );
    for r in rows {
        let stored = match r.cost_usd {
            Some(c) if r.missing_cost > 0 => format!("${c:.4}*"),
            Some(c) => format!("${c:.4}"),
            None => "(no price)".to_string(),
        };
        let hosted = if r.no_tokens > 0 {
            format!("${:.4}*", r.as_hosted_usd)
        } else {
            format!("${:.4}", r.as_hosted_usd)
        };
        println!(
            "{:<28} {:>10} {:>16} {:>13} {:>16}",
            truncate(&r.attribution, 28),
            r.calls,
            r.tokens,
            stored,
            hosted
        );
    }
    let total = rows.last().expect("the total row");
    if total.missing_cost > 0 {
        println!(
            "\nnote: {} call(s) have no stored cost (`*` where some of a row's calls do, \
             `(no price)` where none do): a self-hosted model is unpriced until it is in \
             --price-table, and `evald cost gpu-rate` derives a rate for it.",
            total.missing_cost
        );
    }
    if total.no_tokens > 0 {
        println!(
            "note: `*` in the last column: {} call(s) carry no token counts, so they are not in it.",
            total.no_tokens
        );
    }
}

/// The amortised rate of a self-hosted model: what its GPUs cost over a window, spread over the
/// tokens the traffic evald saw used.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuRate {
    pub models: Vec<String>,
    pub window_hours: f64,
    pub gpus: u32,
    pub gpu_hour_usd: f64,
    /// `gpus × window_hours × gpu_hour_usd`.
    pub gpu_cost_usd: f64,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The first call's start and the last call's end in the window (unix ns).
    pub first_unix_nano: Option<u64>,
    pub last_unix_nano: Option<u64>,
    /// How much of the window the calls span, in `[0, 1]`.
    pub coverage: f64,
    /// An output token costs this many input tokens' worth of GPU time.
    pub output_ratio: f64,
    pub input_per_token: f64,
    pub output_per_token: f64,
}

/// [`GpuRate`] for `models` over the `window` ending at `now_unix_nano`.
#[allow(clippy::too_many_arguments)]
pub async fn gpu_rate(
    store: &Store,
    models: &[String],
    window: std::time::Duration,
    now_unix_nano: u64,
    gpus: u32,
    gpu_hour_usd: f64,
    output_ratio: f64,
) -> anyhow::Result<GpuRate> {
    anyhow::ensure!(
        !models.is_empty(),
        "name the model(s) these GPUs serve with --model"
    );
    anyhow::ensure!(gpus > 0, "--gpus must be at least 1");
    anyhow::ensure!(
        gpu_hour_usd.is_finite() && gpu_hour_usd > 0.0,
        "--gpu-hour-usd must be a positive amount"
    );
    anyhow::ensure!(
        output_ratio.is_finite() && output_ratio > 0.0,
        "--output-ratio must be positive"
    );
    let window_ns = window.as_nanos().min(u64::MAX as u128) as u64;
    anyhow::ensure!(window_ns > 0, "--since must be a positive window");
    let cutoff = now_unix_nano.saturating_sub(window_ns);
    let sql = format!(
        "SELECT COUNT(*) AS calls, SUM(prompt_tokens) AS input, SUM(completion_tokens) AS output, \
                MIN(start_unix_nano) AS first, MAX(end_unix_nano) AS last \
         FROM spans WHERE {} AND model IN ({}) AND start_unix_nano >= {cutoff}",
        crate::latency::LLM_PREDICATE,
        sql_in(models)
    );
    let row = crate::sql::query(store, &sql, 1)
        .await?
        .rows
        .into_iter()
        .next()
        .unwrap_or_default();
    let u = |k: &str| {
        row[k]
            .as_u64()
            .or_else(|| row[k].as_i64().map(|v| v.max(0) as u64))
    };
    let (calls, input, output) = (
        u("calls").unwrap_or(0),
        u("input").unwrap_or(0),
        u("output").unwrap_or(0),
    );
    let weighted = input as f64 + output_ratio * output as f64;
    anyhow::ensure!(
        weighted > 0.0,
        "no tokens from {} in the last {:.1} h: nothing to spread the GPU cost over (check the \
         model names with `evald cost --by model`)",
        models.join(", "),
        window.as_secs_f64() / 3600.0
    );
    let window_hours = window.as_secs_f64() / 3600.0;
    let gpu_cost_usd = f64::from(gpus) * window_hours * gpu_hour_usd;
    let input_per_token = gpu_cost_usd / weighted;
    let (first, last) = (u("first"), u("last"));
    let coverage = match (first, last) {
        (Some(a), Some(b)) if b > a => ((b - a) as f64 / window_ns as f64).min(1.0),
        _ => 0.0,
    };
    Ok(GpuRate {
        models: models.to_vec(),
        window_hours,
        gpus,
        gpu_hour_usd,
        gpu_cost_usd,
        calls,
        input_tokens: input,
        output_tokens: output,
        first_unix_nano: first,
        last_unix_nano: last,
        coverage,
        output_ratio,
        input_per_token,
        output_per_token: input_per_token * output_ratio,
    })
}

/// A price-table overlay entry for each model, at the derived rate.
pub fn gpu_rate_overlay(r: &GpuRate) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for m in &r.models {
        out.insert(
            m.clone(),
            serde_json::json!({
                "input_cost_per_token": r.input_per_token,
                "output_cost_per_token": r.output_per_token,
            }),
        );
    }
    serde_json::Value::Object(out)
}

fn utc(ns: Option<u64>) -> String {
    ns.and_then(|n| chrono::DateTime::from_timestamp((n / 1_000_000_000) as i64, 0))
        .map_or_else(
            || "-".to_string(),
            |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
        )
}

/// One row of [`language_tax`]: a model's answers in one language, and how many tokens each
/// character of them took.
#[derive(Debug, Clone, PartialEq)]
pub struct TaxRow {
    /// [`crate::lang::Detected::code`]: ISO 639-1 where there is one.
    pub language: String,
    pub language_name: String,
    pub model: String,
    pub answers: usize,
    pub chars: u64,
    pub tokens: u64,
    /// This model's tokens per character here over its own in English, when it answered in
    /// English too.
    pub vs_english: Option<f64>,
}

impl TaxRow {
    pub fn tokens_per_char(&self) -> f64 {
        self.tokens as f64 / self.chars.max(1) as f64
    }
}

/// The language tax over `(model, answer text, completion tokens)` samples: per language and
/// model, the characters and tokens of the answers the detector could place (the rest are
/// counted, not guessed). Sorted by language, then cheapest model first.
pub fn tax_rows<'a>(
    samples: impl IntoIterator<Item = (&'a str, &'a str, u64)>,
) -> (Vec<TaxRow>, usize) {
    let mut by: std::collections::BTreeMap<(String, String), TaxRow> = Default::default();
    let mut undetected = 0usize;
    for (model, text, tokens) in samples {
        let Some(d) = crate::lang::detect(text).filter(|d| d.reliable) else {
            undetected += 1;
            continue;
        };
        let row = by
            .entry((d.code.to_string(), model.to_string()))
            .or_insert_with(|| TaxRow {
                language: d.code.to_string(),
                language_name: d.name.to_string(),
                model: model.to_string(),
                answers: 0,
                chars: 0,
                tokens: 0,
                vs_english: None,
            });
        row.answers += 1;
        row.chars += text.chars().count() as u64;
        row.tokens += tokens;
    }
    let english: std::collections::BTreeMap<String, f64> = by
        .values()
        .filter(|r| r.language == "en")
        .map(|r| (r.model.clone(), r.tokens_per_char()))
        .collect();
    let mut rows: Vec<TaxRow> = by.into_values().collect();
    for r in &mut rows {
        r.vs_english = english
            .get(&r.model)
            .filter(|en| **en > 0.0)
            .map(|en| r.tokens_per_char() / en);
    }
    rows.sort_by(|a, b| {
        a.language
            .cmp(&b.language)
            .then(a.tokens_per_char().total_cmp(&b.tokens_per_char()))
    });
    (rows, undetected)
}

/// The language tax from the store: the most recent `limit` model calls in `window` (of
/// `models`, when given) that recorded an answer and its completion tokens.
pub async fn language_tax(
    store: &Store,
    window: std::time::Duration,
    now_unix_nano: u64,
    models: &[String],
    limit: usize,
) -> anyhow::Result<(Vec<TaxRow>, usize, usize)> {
    let since = now_unix_nano.saturating_sub(window.as_nanos() as u64);
    let mut filter = format!(
        "{} AND output_value IS NOT NULL AND completion_tokens > 0 AND start_unix_nano >= {since}",
        crate::latency::LLM_PREDICATE
    );
    if !models.is_empty() {
        filter = format!("{filter} AND model IN ({})", sql_in(models));
    }
    let sql = format!(
        "SELECT model, output_value, completion_tokens FROM spans WHERE {filter} \
         ORDER BY start_unix_nano DESC"
    );
    let resp = crate::sql::query(store, &sql, limit).await?;
    let mut samples: Vec<(String, String, u64)> = Vec::with_capacity(resp.rows.len());
    for r in &resp.rows {
        let (Some(text), Some(tokens)) =
            (r["output_value"].as_str(), r["completion_tokens"].as_u64())
        else {
            continue;
        };
        // An answer offloaded to the blob store is read back; one that is gone is left out.
        let text = if text.starts_with(crate::blob::BLOB_REF_PREFIX) {
            match store
                .get_blob(text)
                .ok()
                .flatten()
                .and_then(|b| String::from_utf8(b).ok())
            {
                Some(t) => t,
                None => continue,
            }
        } else {
            text.to_string()
        };
        let model = r["model"].as_str().unwrap_or("(unknown)").to_string();
        samples.push((model, text, tokens));
    }
    let scanned = samples.len();
    let (rows, undetected) = tax_rows(samples.iter().map(|(m, t, n)| (m.as_str(), t.as_str(), *n)));
    Ok((rows, undetected, scanned))
}

/// `evald cost language-tax`: print tokens per character by language and model.
pub async fn language_tax_command(
    data_dir: &Path,
    window: std::time::Duration,
    models: &[String],
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let (rows, undetected, scanned) = language_tax(&store, window, now, models, limit).await?;
    println!(
        "evald cost language-tax — tokens per character of model answers, the last {:.1} d",
        window.as_secs_f64() / 86_400.0
    );
    println!(
        "{:<9} {:<18} {:<28} {:>8} {:>9} {:>9} {:>9} {:>7}",
        "language", "name", "model", "answers", "chars", "tokens", "tok/char", "vs en"
    );
    for r in &rows {
        println!(
            "{:<9} {:<18} {:<28} {:>8} {:>9} {:>9} {:>9.3} {:>7}",
            r.language,
            truncate(&r.language_name, 18),
            truncate(&r.model, 28),
            r.answers,
            r.chars,
            r.tokens,
            r.tokens_per_char(),
            r.vs_english.map_or("-".to_string(), |x| format!("{x:.2}×"))
        );
    }
    println!(
        "\n{scanned} answer(s) read, {undetected} too short or mixed to place. A character carries \
         more in some scripts than others (a Han character is often a word), so compare models \
         within a language; `vs en` is what the same model charges here per character against \
         English."
    );
    Ok(())
}

/// `evald cost gpu-rate`: print the derived rate and the overlay to paste.
pub async fn gpu_rate_command(
    data_dir: &Path,
    models: &[String],
    window: std::time::Duration,
    gpus: u32,
    gpu_hour_usd: f64,
    output_ratio: f64,
) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let r = gpu_rate(
        &store,
        models,
        window,
        now,
        gpus,
        gpu_hour_usd,
        output_ratio,
    )
    .await?;
    println!(
        "\nevald cost gpu-rate — {} over the last {:.1} h",
        r.models.join(", "),
        r.window_hours
    );
    println!(
        "  GPUs          : {} × ${:.2}/h × {:.1} h = ${:.2}",
        r.gpus, r.gpu_hour_usd, r.window_hours, r.gpu_cost_usd
    );
    println!(
        "  calls         : {} (first {}, last {}; {:.0}% of the window)",
        r.calls,
        utc(r.first_unix_nano),
        utc(r.last_unix_nano),
        r.coverage * 100.0
    );
    println!(
        "  tokens served : {} input, {} output",
        r.input_tokens, r.output_tokens
    );
    println!(
        "  rate          : ${:.4} / ${:.4} per 1M input / output (an output token costs {} input \
         tokens' worth)",
        r.input_per_token * 1e6,
        r.output_per_token * 1e6,
        r.output_ratio
    );
    if r.coverage < 0.9 {
        println!(
            "\nnote: the calls span {:.0}% of the window. The GPUs were paid for all of it, so if \
             evald missed traffic (it started later, or retention dropped it) this rate is too \
             high: use a --since evald saw in full.",
            r.coverage * 100.0
        );
    }
    println!(
        "\nThe GPUs cost the same idle or busy: this is the rate at this traffic, and more \
         traffic lowers it. Add it to your --price-table overlay:\n{}",
        serde_json::to_string_pretty(&gpu_rate_overlay(&r))?
    );
    Ok(())
}

/// `evald cost --reprice-as`: open the store and print what the traffic would have cost hosted.
pub async fn reprice_as_command(
    dimension: Dimension,
    data_dir: &Path,
    limit: usize,
    price_table: Option<&Path>,
    hosted: &str,
    models: &[String],
) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let table = match price_table {
        Some(p) => std::sync::Arc::new(crate::price::PriceTable::load_over_baseline(p)?),
        None => crate::price::PriceTable::baseline(),
    };
    crate::price::warn_if_stale(&table);
    let rows = reprice_as(&store, dimension, limit, &table, hosted, models).await?;
    let price = table
        .lookup(hosted)
        .expect("checked by reprice_as")
        .1
        .base_rates();
    print_as_hosted(dimension, hosted, price, &rows);
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
    #[test]
    fn the_language_tax_is_tokens_per_character_by_language_and_model() {
        let en = "The clinic opens at nine in the morning on Saturdays and closes at noon.";
        let ms =
            "Klinik dibuka pada pukul sembilan pagi setiap hari Sabtu dan ditutup tengah hari.";
        let th = "คลินิกเปิดเวลาเก้าโมงเช้าทุกวันเสาร์และปิดตอนเที่ยง";
        let n = |s: &str| s.chars().count() as u64;
        // Model a: 1 token per 4 characters in English, 1 per 3 in Malay, 1 per character in
        // Thai. Model b: 1 per 2 characters in Thai, and no English answers.
        let samples = [
            ("a", en, n(en) / 4),
            ("a", ms, n(ms) / 3),
            ("a", th, n(th)),
            ("b", th, n(th) / 2),
            ("a", "ok", 1),
        ];
        let (rows, undetected) = super::tax_rows(samples);
        assert_eq!(undetected, 1, "too short to place");
        let row = |lang: &str, model: &str| {
            rows.iter()
                .find(|r| r.language == lang && r.model == model)
                .unwrap_or_else(|| panic!("no {lang}/{model} in {rows:#?}"))
        };
        assert!((row("en", "a").tokens_per_char() - 0.25).abs() < 0.02);
        let thai_a = row("th", "a");
        assert_eq!(thai_a.answers, 1);
        assert!((thai_a.vs_english.unwrap() - 4.0).abs() < 0.3, "{thai_a:?}");
        assert!(row("ms", "a").vs_english.unwrap() > 1.0);
        assert_eq!(
            row("th", "b").vs_english,
            None,
            "b never answered in English"
        );
        // Within a language, the cheaper tokenizer first.
        let thai: Vec<&str> = rows
            .iter()
            .filter(|r| r.language == "th")
            .map(|r| r.model.as_str())
            .collect();
        assert_eq!(thai, ["b", "a"]);
    }

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

    const HOUR_NS: u64 = 3_600_000_000_000;

    /// A model call of `model` at `ts` with `prompt` / `completion` tokens.
    fn call(id: &str, model: &str, ts: u64, prompt: u64, completion: u64) -> crate::NormalizedSpan {
        let mut s = test_span("t", id, ts);
        s.oi_kind = Some("LLM".into());
        s.model = Some(model.into());
        s.tokens.prompt = Some(prompt);
        s.tokens.completion = Some(completion);
        s.tokens.total = Some(prompt + completion);
        s.end_unix_nano = ts + 1_000_000;
        s
    }

    async fn store_with(spans: Vec<crate::NormalizedSpan>) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        store.ingest_batch(spans, &[]).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn the_language_tax_reads_the_stored_answers() {
        let now = 1_000 * HOUR_NS;
        let answer = |id: &str, model: &str, text: &str, tokens: u64, at: u64| {
            let mut s = call(id, model, at, 10, tokens);
            s.output_value = Some(text.into());
            s
        };
        let th = "คลินิกเปิดเวลาเก้าโมงเช้าทุกวันเสาร์และปิดตอนเที่ยง";
        let en = "The clinic opens at nine in the morning on Saturdays and closes at noon.";
        let mut tool = answer("tool", "m", en, 999, now - HOUR_NS);
        tool.oi_kind = Some("TOOL".into()); // not a model's answer
        let (_d, store) = store_with(vec![
            answer("a", "m", en, 18, now - HOUR_NS),
            answer("b", "m", th, 50, now - HOUR_NS),
            answer("c", "other", th, 25, now - HOUR_NS),
            answer("old", "m", th, 9_999, now - 40 * 24 * HOUR_NS), // before the window
            tool,
        ])
        .await;
        let window = std::time::Duration::from_secs(30 * 24 * 3600);
        let (rows, undetected, scanned) =
            language_tax(&store, window, now, &[], 100).await.unwrap();
        assert_eq!((scanned, undetected), (3, 0));
        let thai: Vec<(&str, u64)> = rows
            .iter()
            .filter(|r| r.language == "th")
            .map(|r| (r.model.as_str(), r.tokens))
            .collect();
        assert_eq!(
            thai,
            [("other", 25), ("m", 50)],
            "cheapest first, old and tool calls out"
        );
        let only_m = language_tax(&store, window, now, &["m".into()], 100)
            .await
            .unwrap();
        assert!(only_m.0.iter().all(|r| r.model == "m"));
    }

    #[tokio::test]
    async fn gpu_rate_spreads_what_the_gpus_cost_over_the_tokens_served() {
        let now = 1_000 * HOUR_NS;
        let mut agent = call("agent", "local-qwen", now - HOUR_NS, 9_000, 9_000);
        agent.oi_kind = Some("AGENT".into()); // its usage is its calls': not counted again
        let (_d, store) = store_with(vec![
            call("a", "local-qwen", now - 20 * HOUR_NS, 1_000, 250),
            call("b", "local-qwen", now - 10 * HOUR_NS, 1_000, 250),
            call("c", "local-qwen", now - HOUR_NS, 1_000, 250),
            call("old", "local-qwen", now - 30 * HOUR_NS, 50_000, 50_000), // before the window
            call("other", "gpt-4.1-mini", now - HOUR_NS, 7_000, 7_000),
            agent,
        ])
        .await;
        let window = std::time::Duration::from_secs(24 * 3600);
        let r = gpu_rate(&store, &["local-qwen".into()], window, now, 2, 2.0, 3.0)
            .await
            .unwrap();
        assert_eq!((r.calls, r.input_tokens, r.output_tokens), (3, 3_000, 750));
        // 2 GPUs × $2/h × 24 h = $96, over 3000 + 3 × 750 = 5250 input-token equivalents.
        assert!((r.gpu_cost_usd - 96.0).abs() < 1e-9);
        assert!((r.input_per_token - 96.0 / 5250.0).abs() < 1e-12);
        assert!((r.output_per_token - 3.0 * 96.0 / 5250.0).abs() < 1e-12);
        // The calls span 19 of the 24 hours.
        assert!((r.coverage - (19.0 * 3600.0 + 0.001) / (24.0 * 3600.0)).abs() < 1e-6);
        let overlay = gpu_rate_overlay(&r);
        assert_eq!(
            overlay["local-qwen"]["output_cost_per_token"].as_f64(),
            Some(r.output_per_token)
        );
        // The overlay is a price table evald loads, and it prices the model at the rate.
        let table = crate::price::PriceTable::from_json(&overlay.to_string()).unwrap();
        assert_eq!(
            table.lookup("local-qwen").unwrap().1.base_rates(),
            (r.input_per_token, r.output_per_token)
        );

        let none = gpu_rate(&store, &["nobody".into()], window, now, 1, 1.0, 1.0).await;
        assert!(none
            .unwrap_err()
            .to_string()
            .contains("no tokens from nobody"));
        assert!(gpu_rate(&store, &[], window, now, 1, 1.0, 1.0)
            .await
            .is_err());
        assert!(
            gpu_rate(&store, &["local-qwen".into()], window, now, 0, 1.0, 1.0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reprice_as_prices_the_same_tokens_at_a_hosted_list_price() {
        let mut priced = call("p", "gpt-4.1-mini", 10, 1_000_000, 100_000);
        priced.cost_usd = Some(0.56);
        let (_d, store) = store_with(vec![
            call("a", "local-qwen", 10, 1_000_000, 100_000),
            call("b", "local-qwen", 11, 2_000_000, 200_000),
            priced,
        ])
        .await;
        let table = crate::price::PriceTable::from_json(
            r#"{"hosted-x": {"input_cost_per_token": 0.000002, "output_cost_per_token": 0.000008}}"#,
        )
        .unwrap();
        let rows = reprice_as(&store, Dimension::Model, 10, &table, "hosted-x", &[])
            .await
            .unwrap();
        let get = |k: &str| rows.iter().find(|r| r.attribution == k).unwrap().clone();
        let local = get("local-qwen");
        // 3M input × $2/M + 300k output × $8/M = $6 + $2.40.
        assert_eq!(local.calls, 2);
        assert!((local.as_hosted_usd - 8.4).abs() < 1e-9, "{local:?}");
        assert_eq!((local.cost_usd, local.missing_cost), (None, 2));
        let hosted = get("gpt-4.1-mini");
        assert_eq!(hosted.cost_usd, Some(0.56));
        assert!((hosted.as_hosted_usd - 2.8).abs() < 1e-9);
        let total = rows.last().unwrap();
        assert_eq!(total.attribution, "TOTAL");
        assert_eq!(total.calls, 3);
        assert!((total.as_hosted_usd - 11.2).abs() < 1e-9);

        // Only the self-hosted model's calls.
        let only = reprice_as(
            &store,
            Dimension::Model,
            10,
            &table,
            "hosted-x",
            &["local-qwen".into()],
        )
        .await
        .unwrap();
        assert_eq!(only.len(), 2, "one model and the total");
        assert!(
            reprice_as(&store, Dimension::Model, 10, &table, "unpriced", &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("not in the price table")
        );
    }
}
