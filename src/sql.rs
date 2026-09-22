//! Embedded SQL query engine — DataFusion over the cold Parquet blocks (PoC step 8).
//!
//! evald's cold tier is plain time-partitioned Parquet (`blocks/YYYY/MM/DD/HH/*.parquet`,
//! the flat columnar schema in [`crate::store::cold`]). This module points **DataFusion**
//! — pure-Rust, no C++, so it stays in the single static musl binary — at those blocks and
//! exposes ad-hoc SQL over them, plus the [`crate::Score`] table, so the OTel-native join
//! (`scores.target_id = spans.span_id`) is one query away.
//!
//! Three tables are registered per query:
//! - `cold_spans` — a DataFusion `ListingTable` over the **committed** Parquet blocks the redb
//!   index records (explicit file paths, never a raw dir scan — so an orphan block from a
//!   crashed flush is excluded, matching the REST read path), with real predicate/projection
//!   pushdown straight into the files.
//! - `hot_spans` — an in-memory `MemTable` of the un-compacted hot tier (spans fsynced to
//!   the WAL but not yet flushed to a block).
//! - `scores` — an in-memory `MemTable` of the redb score store.
//!
//! `spans` is then a **view** over `hot_spans ∪ cold_spans`, deduped by
//! `(trace_id, span_id)` (hot wins) — the same hot/cold dedup the read API does, so a
//! query never double-counts or misses a span across an in-flight compaction.
//!
//! Only read statements are accepted — a `Query` (`SELECT` / `WITH`) or a plain `EXPLAIN`
//! (not `EXPLAIN ANALYZE`, which executes). The check is on the parsed statement type, not
//! a leading keyword, so writes / `COPY TO` / `EXPLAIN ANALYZE INSERT` can't slip through:
//! the endpoint is a query surface, not a mutation path into the blocks.

use std::sync::Arc;

use arrow_array::builder::{Float64Builder, StringBuilder, UInt64Builder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::MemTable;
use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::logical_expr::LogicalPlan;
use datafusion::object_store::limit::LimitStore;
use datafusion::object_store::local::LocalFileSystem;
use datafusion::prelude::{DataFrame, SessionContext};
use datafusion::sql::parser::{DFParser, Statement as DfStatement};
use datafusion::sql::sqlparser::ast::Statement as SqlStatement;
use serde::Serialize;

use crate::store::cold;
use crate::{Score, Store};

/// The result of a SQL query: the projected column names (in order) plus the rows as
/// JSON objects keyed by column name. `truncated` is set when more than `row_limit` rows
/// were available and the tail was dropped.
#[derive(Debug, Serialize)]
pub struct SqlResponse {
    pub columns: Vec<String>,
    pub rows: Vec<serde_json::Value>,
    pub row_count: usize,
    pub truncated: bool,
}

/// Run a read-only SQL query against the store, capping the result at `row_limit` rows.
///
/// Registers `spans` (hot ∪ cold, deduped) and `scores`, then executes `sql`. Rejects any
/// non-read statement up front.
pub async fn query(store: &Store, sql: &str, row_limit: usize) -> anyhow::Result<SqlResponse> {
    ensure_read_only(sql)?;

    let ctx = SessionContext::new();
    bound_open_files(&ctx);
    register_spans(&ctx, store).await?;
    register_scores(&ctx, store)?;

    let df = ctx.sql(sql).await?;
    // Column names come from the logical plan schema (correct even for a 0-row result).
    let columns: Vec<String> = df
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();

    let batches = bound_rows(df, row_limit)?.collect().await?;

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    'outer: for batch in &batches {
        for value in batch_to_rows(batch)? {
            if rows.len() >= row_limit {
                truncated = true;
                break 'outer;
            }
            rows.push(value);
        }
    }

    Ok(SqlResponse {
        row_count: rows.len(),
        columns,
        rows,
        truncated,
    })
}

/// Bound the result at `row_limit` rows *during execution*, rather than after it.
///
/// `collect()` materialises every row the plan produces, so truncating afterwards means
/// `SELECT * FROM spans` with a limit of 1 builds the whole store in Arrow memory to return
/// one row — the same collect-then-truncate that [`Store::query`] used to do over the hot
/// tier. A `Limit` in the plan makes the executor stop pulling batches instead.
///
/// It fetches `row_limit + 1` because one row past the cap is what makes `truncated`
/// answerable: it says there was more without paying for the rest. The caller's loop keeps
/// `row_limit` of them and reports the extra as truncation.
fn bound_rows(df: DataFrame, row_limit: usize) -> anyhow::Result<DataFrame> {
    // `EXPLAIN` has to be the root of its own plan — DataFusion rejects a `Limit` above it —
    // and it emits a handful of plan lines, so there is nothing here worth bounding. The
    // caller's loop still truncates that output exactly as it did before.
    if matches!(
        df.logical_plan(),
        LogicalPlan::Explain(_) | LogicalPlan::Analyze(_)
    ) {
        return Ok(df);
    }
    Ok(df.limit(0, Some(row_limit.saturating_add(1)))?)
}

/// Allow only a single **read** statement. A leading-keyword check is not enough:
/// DataFusion can `INSERT` / `COPY TO` / `CREATE EXTERNAL TABLE` (and a `ListingTable` is
/// itself insertable), and `EXPLAIN ANALYZE` *executes* its inner plan — so
/// `EXPLAIN ANALYZE INSERT …` would slip past a prefix guard. Parse the statement and gate
/// on its type instead: a top-level `Query`, or a plain `EXPLAIN` (no `ANALYZE`) of a read
/// statement. Everything else is rejected, keeping the blocks-write path the commit
/// protocol rather than SQL.
///
/// Public because it is the canonical read-only gate for *any* SQL surface over evald's
/// open data formats — downstream query fronts (e.g. a consolidated multi-node query
/// node) must apply the exact same guard rather than re-derive a weaker one.
pub fn ensure_read_only(sql: &str) -> anyhow::Result<()> {
    let stmts =
        DFParser::parse_sql(sql).map_err(|e| anyhow::anyhow!("could not parse SQL: {e}"))?;
    if stmts.len() != 1 {
        anyhow::bail!("exactly one SQL statement is allowed (got {})", stmts.len());
    }
    if !statement_is_read_only(&stmts[0]) {
        anyhow::bail!(
            "only read queries are allowed — SELECT / WITH, or EXPLAIN (without ANALYZE) over a read query"
        );
    }
    Ok(())
}

/// A top-level `Query`, or a non-analyzing `EXPLAIN` whose inner statement is itself
/// read-only. `COPY TO`, `CREATE EXTERNAL TABLE`, `RESET`, any DML/DDL, and
/// `EXPLAIN ANALYZE` (which runs the inner plan) are all read-write → `false`.
fn statement_is_read_only(stmt: &DfStatement) -> bool {
    match stmt {
        DfStatement::Statement(s) => matches!(s.as_ref(), SqlStatement::Query(_)),
        DfStatement::Explain(e) => !e.analyze && statement_is_read_only(&e.statement),
        _ => false,
    }
}

/// The most cold blocks a query holds open at once, whatever the block count.
///
/// Every block is registered as its own listing URL (see [`register_spans`]), and the scan
/// opens them concurrently, so the open-file working set tracked the block count and a full
/// scan over a long-lived store died with `EMFILE` (`docs/SOAK.md`, "The ceiling this gate
/// found — and what fixed it"). A [`LimitStore`] puts a semaphore in front of every object-store operation, so
/// at most this many are in flight — and therefore at most this many blocks are open —
/// however many the index names. Measured: without it, a scan of 2,000 blocks peaked over
/// 1,000 descriptors and failed under `ulimit -n 256`; with it the peak is exactly this
/// number, and the same scan runs under `ulimit -n 96`.
///
/// 64 is chosen to leave a stock `ulimit -n 1024` shell room for the runtime, the stdio and
/// the redb files, while staying wide enough that the scan is never open-bound in practice.
/// `tests/fd_ceiling.rs` pins the claim: 800 blocks scanned under a 192-descriptor limit.
///
/// Merging (`store::merge`) is what bounds the block *count*; this bounds the *descriptors*
/// a scan uses even before merging has caught up.
pub const COLD_SCAN_MAX_OPEN_FILES: usize = 64;

/// Route `file://` reads through a concurrency-limited local store (see
/// [`COLD_SCAN_MAX_OPEN_FILES`]).
fn bound_open_files(ctx: &SessionContext) {
    let limited = LimitStore::new(LocalFileSystem::new(), COLD_SCAN_MAX_OPEN_FILES);
    ctx.runtime_env().register_object_store(
        ObjectStoreUrl::local_filesystem().as_ref(),
        Arc::new(limited),
    );
}

/// The listing URLs for `block_paths`, or `None` if one of them has been reclaimed since the
/// index named it — the caller re-lists and tries again.
fn cold_urls(block_paths: &[std::path::PathBuf]) -> anyhow::Result<Option<Vec<ListingTableUrl>>> {
    let mut urls = Vec::with_capacity(block_paths.len());
    for p in block_paths {
        // Absolutize so the file:// URL is well-formed regardless of a relative --data-dir.
        let abs = match std::fs::canonicalize(p) {
            Ok(abs) => abs,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %p.display(), "cold block vanished before the scan; re-listing");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let s = abs
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("block path is not valid UTF-8: {}", abs.display()))?;
        // Windows: canonicalize returns a verbatim path (`\\?\C:\...`), which
        // `ListingTableUrl::parse` misreads as a relative path. Hand it a
        // well-formed file:// URL instead.
        #[cfg(windows)]
        let s = &format!(
            "file:///{}",
            s.trim_start_matches(r"\\?\").replace('\\', "/")
        );
        urls.push(ListingTableUrl::parse(s)?);
    }
    Ok(Some(urls))
}

/// Register `cold_spans` (ListingTable over the blocks), `hot_spans` (MemTable), and the
/// deduped `spans` view over their union.
async fn register_spans(ctx: &SessionContext, store: &Store) -> anyhow::Result<()> {
    let schema = cold::schema();

    // cold_spans: register the COMMITTED blocks the index knows about, as explicit file URLs.
    // A directory/glob `ListingTable` over blocks/ is unreliable for the time-partitioned tree
    // (blocks/YYYY/MM/DD/HH/*.parquet) — DataFusion's directory listing misses the nested
    // files — and a raw dir scan could also surface an orphan block from a crashed flush. Using
    // the index's committed paths fixes both, and matches the REST read path exactly. With no
    // blocks yet, register an empty MemTable carrying the schema so `spans` is always queryable.
    //
    // A merge can take a block out of the index between the listing and the registration
    // below, and reclaim the file once its grace elapses. The block's rows are in the merged
    // block, which a re-listing includes — so re-list once rather than fail the query on a
    // path the index no longer names. This mirrors the same retry in `Store::query`; without
    // it a long scan that straddles a merge dies on `canonicalize`. A second miss is an
    // error, not an empty table: returning the hot tier alone and calling it the answer
    // would be a silently wrong result, which is worse than a query the caller can retry.
    let urls = match cold_urls(&store.cold_block_paths()?)? {
        Some(urls) => urls,
        None => cold_urls(&store.cold_block_paths()?)?.ok_or_else(|| {
            anyhow::anyhow!(
                "a cold block was reclaimed twice while registering the scan; retry the query"
            )
        })?,
    };
    if urls.is_empty() {
        let empty = MemTable::try_new(schema.clone(), vec![vec![]])?;
        ctx.register_table("cold_spans", Arc::new(empty))?;
    } else {
        // No per-file statistics at planning time: collecting them reads every block's
        // footer on every query — O(blocks) I/O before a byte of the scan — and nothing
        // here uses them (the `spans` view is an anti-join, never a stats-only aggregate).
        // Row-group pruning from a `WHERE` still happens at scan time, per file.
        let options =
            ListingOptions::new(Arc::new(ParquetFormat::default())).with_collect_stat(false);
        let config = ListingTableConfig::new_with_multi_paths(urls)
            .with_listing_options(options)
            .with_schema(schema.clone());
        let table = ListingTable::try_new(config)?;
        ctx.register_table("cold_spans", Arc::new(table))?;
    }

    // hot_spans: the un-compacted in-memory tier.
    let hot = store.hot_spans();
    let hot_batches = if hot.is_empty() {
        vec![]
    } else {
        vec![cold::build_batch(&hot)?]
    };
    let hot_table = MemTable::try_new(schema, vec![hot_batches])?;
    ctx.register_table("hot_spans", Arc::new(hot_table))?;

    // spans = hot ∪ cold, deduped by (trace_id, span_id) — hot wins, matching the read API.
    ctx.sql(
        "CREATE VIEW spans AS \
         SELECT * FROM hot_spans \
         UNION ALL \
         SELECT * FROM cold_spans c \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM hot_spans h \
             WHERE h.trace_id = c.trace_id AND h.span_id = c.span_id \
         )",
    )
    .await?;
    Ok(())
}

/// Register the `scores` table (the redb score store, materialized in memory).
///
/// This loads the whole score store into a `MemTable` per query — fine at PoC scale (scores
/// are far smaller than spans, and `hot_spans` is bounded by the WAL seal threshold), but a
/// streaming redb-backed `TableProvider` with filter/projection pushdown is the GA path for
/// a very large score store. The big tier (`cold_spans`) already streams from Parquet with
/// pushdown, so only the score store is fully materialized today.
fn register_scores(ctx: &SessionContext, store: &Store) -> anyhow::Result<()> {
    let scores = store.list_scores(usize::MAX)?;
    let schema = scores_schema();
    let batches = if scores.is_empty() {
        vec![]
    } else {
        vec![scores_batch(&scores, schema.clone())?]
    };
    let table = MemTable::try_new(schema, vec![batches])?;
    ctx.register_table("scores", Arc::new(table))?;
    Ok(())
}

/// Flat columnar schema for the `scores` SQL table (mirrors [`Score`]'s JSON shape; the
/// `ScoreTarget` enum is flattened to `target_type` + `target_id`).
///
/// Public because this is also the **score consolidation format**: a score export written
/// as Parquet with this exact schema joins `spans` on `target_id = span_id` anywhere —
/// on this node, in DuckDB, or on a consolidated query plane.
pub fn scores_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("target_type", DataType::Utf8, false),
        Field::new("target_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("num_value", DataType::Float64, true),
        Field::new("str_value", DataType::Utf8, true),
        Field::new("data_type", DataType::Utf8, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("comment", DataType::Utf8, true),
        Field::new("config_id", DataType::Utf8, true),
        Field::new("ts_unix_nano", DataType::UInt64, false),
    ]))
}

/// Build a `scores` RecordBatch matching [`scores_schema`] (the score consolidation
/// format — see [`scores_schema`] for why this is public).
pub fn scores_batch(scores: &[Score], schema: SchemaRef) -> anyhow::Result<RecordBatch> {
    let mut id = StringBuilder::new();
    let mut target_type = StringBuilder::new();
    let mut target_id = StringBuilder::new();
    let mut name = StringBuilder::new();
    let mut num_value = Float64Builder::new();
    let mut str_value = StringBuilder::new();
    let mut data_type = StringBuilder::new();
    let mut source = StringBuilder::new();
    let mut comment = StringBuilder::new();
    let mut config_id = StringBuilder::new();
    let mut ts = UInt64Builder::new();

    for s in scores {
        let (tt, tid) = target_parts(&s.target);
        id.append_value(&s.id);
        target_type.append_value(tt);
        target_id.append_value(tid);
        name.append_value(&s.name);
        num_value.append_option(s.num_value);
        str_value.append_option(s.str_value.as_deref());
        data_type.append_value(data_type_str(s.data_type));
        source.append_value(source_str(s.source));
        comment.append_option(s.comment.as_deref());
        config_id.append_option(s.config_id.as_deref());
        ts.append_value(s.ts_unix_nano);
    }

    let arrays: Vec<ArrayRef> = vec![
        Arc::new(id.finish()),
        Arc::new(target_type.finish()),
        Arc::new(target_id.finish()),
        Arc::new(name.finish()),
        Arc::new(num_value.finish()),
        Arc::new(str_value.finish()),
        Arc::new(data_type.finish()),
        Arc::new(source.finish()),
        Arc::new(comment.finish()),
        Arc::new(config_id.finish()),
        Arc::new(ts.finish()),
    ];
    Ok(RecordBatch::try_new(schema, arrays)?)
}

fn target_parts(t: &crate::ScoreTarget) -> (&'static str, &str) {
    match t {
        crate::ScoreTarget::Trace(id) => ("trace", id),
        crate::ScoreTarget::Span(id) => ("span", id),
        crate::ScoreTarget::Session(id) => ("session", id),
        crate::ScoreTarget::Run(id) => ("run", id),
    }
}

fn data_type_str(d: crate::DataType) -> &'static str {
    match d {
        crate::DataType::Numeric => "numeric",
        crate::DataType::Categorical => "categorical",
        crate::DataType::Boolean => "boolean",
        crate::DataType::Text => "text",
    }
}

fn source_str(s: crate::ScoreSource) -> &'static str {
    match s {
        crate::ScoreSource::Eval => "eval",
        crate::ScoreSource::Human => "human",
        crate::ScoreSource::Api => "api",
    }
}

/// Serialize one Arrow `RecordBatch` to a `Vec` of JSON row objects (column → value).
/// Public so downstream SQL surfaces return the identical `{ columns, rows }` wire shape.
pub fn batch_to_rows(batch: &RecordBatch) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut buf = Vec::new();
    let mut writer = arrow_json::ArrayWriter::new(&mut buf);
    writer.write(batch)?;
    writer.finish()?;
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;
    use crate::{DataType, Score, ScoreSource, ScoreTarget, StoreConfig};

    async fn store_with(
        spans: Vec<crate::NormalizedSpan>,
        scores: Vec<Score>,
    ) -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        if !spans.is_empty() {
            store.append(spans).await.unwrap();
        }
        if !scores.is_empty() {
            store.put_scores(&scores).unwrap();
        }
        (store, dir)
    }

    fn score_on_span(id: &str, span: &str, name: &str, v: f64) -> Score {
        Score {
            id: id.to_string(),
            target: ScoreTarget::Span(span.to_string()),
            name: name.to_string(),
            num_value: Some(v),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: 1,
        }
    }

    #[tokio::test]
    async fn rejects_non_read_statements() {
        let (store, _d) = store_with(vec![], vec![]).await;
        for bad in [
            "DROP TABLE spans",
            "INSERT INTO spans VALUES (1)",
            "CREATE TABLE x(a int)",
        ] {
            assert!(
                query(&store, bad, 100).await.is_err(),
                "should reject: {bad}"
            );
        }
    }

    #[tokio::test]
    async fn read_only_guard_blocks_execute_and_write_vectors() {
        let (store, _d) = store_with(vec![], vec![]).await;
        // EXPLAIN ANALYZE *executes* the inner plan; COPY TO / CREATE EXTERNAL TABLE /
        // INSERT mutate; a second statement smuggles a write. All must be rejected — a
        // leading-keyword check would have let EXPLAIN ANALYZE and the comment-prefixed
        // write through.
        for bad in [
            "EXPLAIN ANALYZE SELECT * FROM spans",
            "INSERT INTO cold_spans SELECT * FROM hot_spans",
            "COPY (SELECT 1) TO 'pwned.parquet'",
            "CREATE EXTERNAL TABLE t STORED AS PARQUET LOCATION '/tmp/x'",
            "SELECT 1; DROP TABLE spans",
            "-- comment\nDROP TABLE spans",
        ] {
            assert!(
                query(&store, bad, 100).await.is_err(),
                "should reject: {bad}"
            );
        }
        // A plain EXPLAIN (no ANALYZE) only plans — never executes — so it is allowed.
        assert!(query(&store, "EXPLAIN SELECT * FROM spans", 100)
            .await
            .is_ok());
        // A CTE read is allowed.
        assert!(
            query(&store, "WITH t AS (SELECT 1 AS x) SELECT x FROM t", 100)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn selects_hot_spans() {
        let mut a = test_span("trace1", "span1", 1_700_000_000_000_000_000);
        a.model = Some("gpt-4o".into());
        let b = test_span("trace1", "span2", 1_700_000_000_500_000_000);
        let (store, _d) = store_with(vec![a, b], vec![]).await;

        let r = query(
            &store,
            "SELECT span_id, model FROM spans ORDER BY span_id",
            100,
        )
        .await
        .unwrap();
        assert_eq!(r.row_count, 2);
        assert_eq!(r.columns, vec!["span_id", "model"]);
        assert_eq!(r.rows[0]["span_id"], "span1");
        assert_eq!(r.rows[0]["model"], "gpt-4o");
    }

    #[tokio::test]
    async fn sql_reads_cold_parquet_after_compaction() {
        // Regression: spans compacted to the time-partitioned cold tier
        // (blocks/YYYY/MM/DD/HH/*.parquet) must remain visible to SQL. The ListingTable over
        // that nested tree previously matched zero files, silently dropping all compacted data.
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
            .append(vec![test_span("aa", "01", 1_700_000_000_000_000_000)])
            .await
            .unwrap();
        store
            .append(vec![test_span("aa", "02", 1_700_000_000_500_000_000)])
            .await
            .unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        assert!(
            store.hot_spans().is_empty(),
            "hot tier should be flushed to cold"
        );

        let r = query(
            &store,
            "SELECT COUNT(*) AS n, COUNT(DISTINCT span_id) AS d FROM spans",
            100,
        )
        .await
        .unwrap();
        assert_eq!(
            r.rows[0]["n"], 2,
            "SQL must see both spans from cold Parquet"
        );
        assert_eq!(r.rows[0]["d"], 2);
    }

    #[tokio::test]
    async fn empty_store_returns_no_rows_not_an_error() {
        let (store, _d) = store_with(vec![], vec![]).await;
        let r = query(&store, "SELECT * FROM spans", 100).await.unwrap();
        assert_eq!(r.row_count, 0);
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn aggregates_and_joins_spans_with_scores() {
        let mut a = test_span("t", "span1", 1_700_000_000_000_000_000);
        a.model = Some("gpt-4o".into());
        let mut b = test_span("t", "span2", 1_700_000_000_100_000_000);
        b.model = Some("gpt-4o".into());
        let scores = vec![
            score_on_span("s1", "span1", "exact_match", 1.0),
            score_on_span("s2", "span2", "exact_match", 0.0),
        ];
        let (store, _d) = store_with(vec![a, b], scores).await;

        let sql = "SELECT s.model, COUNT(*) AS n, AVG(sc.num_value) AS avg_score \
                   FROM spans s JOIN scores sc \
                     ON sc.target_id = s.span_id AND sc.target_type = 'span' \
                   GROUP BY s.model";
        let r = query(&store, sql, 100).await.unwrap();
        assert_eq!(r.row_count, 1);
        assert_eq!(r.rows[0]["model"], "gpt-4o");
        assert_eq!(r.rows[0]["n"], 2);
        assert_eq!(r.rows[0]["avg_score"], 0.5);
    }

    #[tokio::test]
    async fn row_limit_truncates() {
        let spans: Vec<_> = (0..5)
            .map(|i| test_span("t", &format!("span{i}"), 1_700_000_000_000_000_000 + i))
            .collect();
        let (store, _d) = store_with(spans, vec![]).await;
        let r = query(&store, "SELECT span_id FROM spans", 3).await.unwrap();
        assert_eq!(r.row_count, 3);
        assert!(r.truncated);
    }

    /// `row_limit_truncates` above pins the *answer*, and passes either way — a
    /// collect-then-truncate implementation returns exactly the same rows. What it cannot
    /// see is whether the whole result was built in memory first. The bound is only worth
    /// anything if it is in the plan, so that is what this asserts.
    #[tokio::test]
    async fn the_row_limit_is_in_the_plan_not_applied_after_collect() {
        let ctx = SessionContext::new();
        let df = ctx.sql("SELECT 1 AS n").await.unwrap();
        let plan = bound_rows(df, 3)
            .unwrap()
            .logical_plan()
            .display_indent()
            .to_string();
        assert!(
            plan.contains("Limit:") && plan.contains("fetch=4"),
            "the plan should carry a fetch of row_limit + 1; got:\n{plan}"
        );
    }

    /// A bounded read must not report truncation when the result fits exactly. The extra row
    /// the plan fetches is a probe, not part of the answer.
    #[tokio::test]
    async fn an_exact_fit_is_not_reported_as_truncated() {
        let spans: Vec<_> = (0..3)
            .map(|i| test_span("t", &format!("span{i}"), 1_700_000_000_000_000_000 + i))
            .collect();
        let (store, _d) = store_with(spans, vec![]).await;
        let r = query(&store, "SELECT span_id FROM spans", 3).await.unwrap();
        assert_eq!(r.row_count, 3);
        assert!(
            !r.truncated,
            "three rows under a limit of three is not truncated"
        );
    }

    #[tokio::test]
    async fn dedups_across_hot_and_cold() {
        // Same span flushed to cold, then re-appended to hot — the dedup view keeps one.
        let s = test_span("t", "dup", 1_700_000_000_000_000_000);
        let (store, _d) = store_with(vec![s.clone()], vec![]).await;
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap(); // the "dup" span is now in a cold block
        store.append(vec![s]).await.unwrap(); // and re-appended into the hot tier

        let r = query(
            &store,
            "SELECT COUNT(*) AS n FROM spans WHERE span_id = 'dup'",
            100,
        )
        .await
        .unwrap();
        assert_eq!(r.rows[0]["n"], 1);
    }
}
