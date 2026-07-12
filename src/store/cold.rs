//! Cold tier — time-partitioned Parquet blocks (PoC step 4).
//!
//! Spans are written as a flat, typed columnar schema (the LLM-semantic fields become
//! real columns, so the blocks are directly queryable by external tools — `duckdb`,
//! `pandas`, DataFusion later — which is part of the pitch). `raw_attributes` rides
//! along as a JSON string column for lossless reconstruction.
//!
//! Block publication follows the commit protocol (PLAN.md §1.3 steps 1): write to a
//! temp path, fsync the file, atomically rename into place, fsync the parent dir — so a
//! block is either fully present or absent, never half-written.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::Arc;

use arrow_array::builder::{Float64Builder, Int32Builder, StringBuilder, UInt64Builder};
use arrow_array::{
    Array, ArrayRef, Float64Array, Int32Array, RecordBatch, StringArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::model::{Dialect, NormalizedSpan, Tokens};

/// What a freshly-written block contributes to the index at commit time.
pub struct BlockMeta {
    /// Path relative to the data dir (e.g. `blocks/2026/06/24/05/00..03-0.parquet`).
    pub rel_path: String,
    pub max_start_unix_nano: u64,
    pub trace_ids: BTreeSet<String>,
}

/// The flat columnar schema. Column order is load-bearing for [`decode_batch`].
pub fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("dialect", DataType::Utf8, false),
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("parent_span_id", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("otel_kind", DataType::Int32, false),
        Field::new("oi_kind", DataType::Utf8, true),
        Field::new("start_unix_nano", DataType::UInt64, false),
        Field::new("end_unix_nano", DataType::UInt64, false),
        Field::new("status_code", DataType::Int32, false),
        Field::new("status_message", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("provider", DataType::Utf8, true),
        Field::new("prompt_tokens", DataType::UInt64, true),
        Field::new("completion_tokens", DataType::UInt64, true),
        Field::new("total_tokens", DataType::UInt64, true),
        Field::new("cache_read_tokens", DataType::UInt64, true),
        Field::new("cache_write_tokens", DataType::UInt64, true),
        Field::new("reasoning_tokens", DataType::UInt64, true),
        Field::new("cost_usd", DataType::Float64, true),
        Field::new("input_value", DataType::Utf8, true),
        Field::new("output_value", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("user_id", DataType::Utf8, true),
        Field::new("service_name", DataType::Utf8, true),
        Field::new("scope_name", DataType::Utf8, true),
        Field::new("scope_version", DataType::Utf8, true),
        Field::new("raw_attributes_json", DataType::Utf8, false),
    ]))
}

/// UTC time-partition key `YYYY/MM/DD/HH` derived from a span's start time.
///
/// An out-of-range / overflowing `start_unix_nano` falls back to the epoch partition
/// (`1970/01/01/00`) so a stray span is mis-partitioned rather than dropped or crashing
/// — but it is logged so the bad input is debuggable.
pub fn partition_of(start_unix_nano: u64) -> String {
    let secs = (start_unix_nano / 1_000_000_000) as i64;
    let nanos = (start_unix_nano % 1_000_000_000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_else(|| {
        tracing::warn!(
            start_unix_nano,
            "span start time out of representable range — bucketing into the epoch partition"
        );
        chrono::DateTime::UNIX_EPOCH
    });
    dt.format("%Y/%m/%d/%H").to_string()
}

/// Write one block (all spans assumed in the same `partition`) durably, returning what
/// the index needs. `file_stem` must be unique within the partition.
pub fn write_block(
    data_dir: &Path,
    blocks_subdir: &str,
    partition: &str,
    file_stem: &str,
    spans: &[NormalizedSpan],
) -> io::Result<BlockMeta> {
    let rel_dir = format!("{blocks_subdir}/{partition}");
    let abs_dir = data_dir.join(&rel_dir);
    fs::create_dir_all(&abs_dir)?;

    let rel_path = format!("{rel_dir}/{file_stem}.parquet");
    let abs_path = data_dir.join(&rel_path);
    let tmp_path = abs_dir.join(format!("{file_stem}.parquet.tmp"));

    let batch = build_batch(spans)?;
    {
        let file = File::create(&tmp_path)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let mut writer =
            ArrowWriter::try_new(file, schema(), Some(props)).map_err(io::Error::other)?;
        writer.write(&batch).map_err(io::Error::other)?;
        let file = writer.into_inner().map_err(io::Error::other)?; // writes footer, returns the File
        file.sync_all()?; // fsync the parquet file before publishing it
    }
    // Atomic publish: rename into place, then fsync the directory entry.
    fs::rename(&tmp_path, &abs_path)?;
    fsync_dir(&abs_dir)?;

    Ok(BlockMeta {
        rel_path,
        max_start_unix_nano: spans.iter().map(|s| s.start_unix_nano).max().unwrap_or(0),
        trace_ids: spans.iter().map(|s| s.trace_id.clone()).collect(),
    })
}

/// Read all spans from a block.
pub fn read_block(abs_path: &Path) -> io::Result<Vec<NormalizedSpan>> {
    let file = File::open(abs_path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(io::Error::other)?
        .build()
        .map_err(io::Error::other)?;
    let mut out = Vec::new();
    for batch in reader {
        decode_batch(&batch.map_err(io::Error::other)?, &mut out)?;
    }
    Ok(out)
}

// Windows: `File::open(dir)` fails with os error 5 (needs FILE_FLAG_BACKUP_SEMANTICS);
// NTFS journals metadata itself, so skip the directory fsync there (see store/wal.rs).
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Build an Arrow `RecordBatch` (one row per span) matching [`schema`]. Shared with the
/// DataFusion query engine (PoC step 8), which wraps the hot tier in an in-memory table.
// Public like `schema()`: the cold tier is an open format and the separately-licensed
// fleet sidecars build/merge blocks with the same batch builder the engine uses.
pub fn build_batch(spans: &[NormalizedSpan]) -> io::Result<RecordBatch> {
    let mut dialect = StringBuilder::new();
    let mut trace_id = StringBuilder::new();
    let mut span_id = StringBuilder::new();
    let mut parent_span_id = StringBuilder::new();
    let mut name = StringBuilder::new();
    let mut otel_kind = Int32Builder::new();
    let mut oi_kind = StringBuilder::new();
    let mut start = UInt64Builder::new();
    let mut end = UInt64Builder::new();
    let mut status_code = Int32Builder::new();
    let mut status_message = StringBuilder::new();
    let mut model = StringBuilder::new();
    let mut provider = StringBuilder::new();
    let mut prompt = UInt64Builder::new();
    let mut completion = UInt64Builder::new();
    let mut total = UInt64Builder::new();
    let mut cache_read = UInt64Builder::new();
    let mut cache_write = UInt64Builder::new();
    let mut reasoning = UInt64Builder::new();
    let mut cost = Float64Builder::new();
    let mut input_value = StringBuilder::new();
    let mut output_value = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut user_id = StringBuilder::new();
    let mut service_name = StringBuilder::new();
    let mut scope_name = StringBuilder::new();
    let mut scope_version = StringBuilder::new();
    let mut raw_attributes_json = StringBuilder::new();

    for s in spans {
        dialect.append_value(s.dialect.as_str());
        trace_id.append_value(&s.trace_id);
        span_id.append_value(&s.span_id);
        parent_span_id.append_option(s.parent_span_id.as_deref());
        name.append_value(&s.name);
        otel_kind.append_value(s.otel_kind);
        oi_kind.append_option(s.oi_kind.as_deref());
        start.append_value(s.start_unix_nano);
        end.append_value(s.end_unix_nano);
        status_code.append_value(s.status_code);
        status_message.append_option(s.status_message.as_deref());
        model.append_option(s.model.as_deref());
        provider.append_option(s.provider.as_deref());
        prompt.append_option(s.tokens.prompt);
        completion.append_option(s.tokens.completion);
        total.append_option(s.tokens.total);
        cache_read.append_option(s.tokens.cache_read);
        cache_write.append_option(s.tokens.cache_write);
        reasoning.append_option(s.tokens.reasoning);
        cost.append_option(s.cost_usd);
        input_value.append_option(s.input_value.as_deref());
        output_value.append_option(s.output_value.as_deref());
        session_id.append_option(s.session_id.as_deref());
        user_id.append_option(s.user_id.as_deref());
        service_name.append_option(s.service_name.as_deref());
        scope_name.append_option(s.scope_name.as_deref());
        scope_version.append_option(s.scope_version.as_deref());
        let raw = serde_json::to_string(&s.raw_attributes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        raw_attributes_json.append_value(&raw);
    }

    let arrays: Vec<ArrayRef> = vec![
        Arc::new(dialect.finish()),
        Arc::new(trace_id.finish()),
        Arc::new(span_id.finish()),
        Arc::new(parent_span_id.finish()),
        Arc::new(name.finish()),
        Arc::new(otel_kind.finish()),
        Arc::new(oi_kind.finish()),
        Arc::new(start.finish()),
        Arc::new(end.finish()),
        Arc::new(status_code.finish()),
        Arc::new(status_message.finish()),
        Arc::new(model.finish()),
        Arc::new(provider.finish()),
        Arc::new(prompt.finish()),
        Arc::new(completion.finish()),
        Arc::new(total.finish()),
        Arc::new(cache_read.finish()),
        Arc::new(cache_write.finish()),
        Arc::new(reasoning.finish()),
        Arc::new(cost.finish()),
        Arc::new(input_value.finish()),
        Arc::new(output_value.finish()),
        Arc::new(session_id.finish()),
        Arc::new(user_id.finish()),
        Arc::new(service_name.finish()),
        Arc::new(scope_name.finish()),
        Arc::new(scope_version.finish()),
        Arc::new(raw_attributes_json.finish()),
    ];
    RecordBatch::try_new(schema(), arrays).map_err(io::Error::other)
}

/// Reconstruct [`NormalizedSpan`]s from a `RecordBatch` (the inverse of [`build_batch`]).
fn decode_batch(batch: &RecordBatch, out: &mut Vec<NormalizedSpan>) -> io::Result<()> {
    let dialect = str_col(batch, 0)?;
    let trace_id = str_col(batch, 1)?;
    let span_id = str_col(batch, 2)?;
    let parent_span_id = str_col(batch, 3)?;
    let name = str_col(batch, 4)?;
    let otel_kind = i32_col(batch, 5)?;
    let oi_kind = str_col(batch, 6)?;
    let start = u64_col(batch, 7)?;
    let end = u64_col(batch, 8)?;
    let status_code = i32_col(batch, 9)?;
    let status_message = str_col(batch, 10)?;
    let model = str_col(batch, 11)?;
    let provider = str_col(batch, 12)?;
    let prompt = u64_col(batch, 13)?;
    let completion = u64_col(batch, 14)?;
    let total = u64_col(batch, 15)?;
    let cache_read = u64_col(batch, 16)?;
    let cache_write = u64_col(batch, 17)?;
    let reasoning = u64_col(batch, 18)?;
    let cost = f64_col(batch, 19)?;
    let input_value = str_col(batch, 20)?;
    let output_value = str_col(batch, 21)?;
    let session_id = str_col(batch, 22)?;
    let user_id = str_col(batch, 23)?;
    let service_name = str_col(batch, 24)?;
    let scope_name = str_col(batch, 25)?;
    let scope_version = str_col(batch, 26)?;
    let raw_attributes_json = str_col(batch, 27)?;

    for i in 0..batch.num_rows() {
        let raw_attributes = serde_json::from_str(raw_attributes_json.value(i))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        out.push(NormalizedSpan {
            dialect: Dialect::from_str_lenient(dialect.value(i)),
            trace_id: trace_id.value(i).to_string(),
            span_id: span_id.value(i).to_string(),
            parent_span_id: opt_str(parent_span_id, i),
            name: name.value(i).to_string(),
            otel_kind: otel_kind.value(i),
            oi_kind: opt_str(oi_kind, i),
            start_unix_nano: start.value(i),
            end_unix_nano: end.value(i),
            status_code: status_code.value(i),
            status_message: opt_str(status_message, i),
            model: opt_str(model, i),
            provider: opt_str(provider, i),
            tokens: Tokens {
                prompt: opt_u64(prompt, i),
                completion: opt_u64(completion, i),
                total: opt_u64(total, i),
                cache_read: opt_u64(cache_read, i),
                cache_write: opt_u64(cache_write, i),
                reasoning: opt_u64(reasoning, i),
            },
            cost_usd: opt_f64(cost, i),
            input_value: opt_str(input_value, i),
            output_value: opt_str(output_value, i),
            session_id: opt_str(session_id, i),
            user_id: opt_str(user_id, i),
            service_name: opt_str(service_name, i),
            scope_name: opt_str(scope_name, i),
            scope_version: opt_str(scope_version, i),
            raw_attributes,
        });
    }
    Ok(())
}

fn str_col(batch: &RecordBatch, idx: usize) -> io::Result<&StringArray> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| io::Error::other(format!("column {idx} is not Utf8")))
}
fn u64_col(batch: &RecordBatch, idx: usize) -> io::Result<&UInt64Array> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| io::Error::other(format!("column {idx} is not UInt64")))
}
fn i32_col(batch: &RecordBatch, idx: usize) -> io::Result<&Int32Array> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| io::Error::other(format!("column {idx} is not Int32")))
}
fn f64_col(batch: &RecordBatch, idx: usize) -> io::Result<&Float64Array> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| io::Error::other(format!("column {idx} is not Float64")))
}

fn opt_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_string())
    }
}
fn opt_u64(arr: &UInt64Array, i: usize) -> Option<u64> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i))
    }
}
fn opt_f64(arr: &Float64Array, i: usize) -> Option<f64> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_span;

    #[test]
    fn partition_key_is_utc_hour() {
        // 2023-11-14T22:13:20Z
        assert_eq!(partition_of(1_700_000_000_000_000_000), "2023/11/14/22");
    }

    #[test]
    fn write_then_read_roundtrips_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = test_span("aa", "01", 1_700_000_000_000_000_000);
        a.model = Some("claude-opus-4-8".into());
        a.provider = Some("anthropic".into());
        a.oi_kind = Some("LLM".into());
        a.tokens = Tokens {
            prompt: Some(1875),
            completion: Some(432),
            total: Some(2307),
            cache_write: Some(1024),
            ..Tokens::default()
        };
        a.cost_usd = Some(0.0123);
        a.raw_attributes
            .insert("k".into(), serde_json::json!({"nested": [1, 2, 3]}));
        let b = test_span("aa", "02", 1_700_000_000_500_000_000); // minimal / mostly-null

        let meta = write_block(
            dir.path(),
            "blocks",
            "2023/11/14/22",
            "0000-0",
            &[a.clone(), b.clone()],
        )
        .unwrap();
        assert_eq!(meta.rel_path, "blocks/2023/11/14/22/0000-0.parquet");
        assert_eq!(meta.max_start_unix_nano, 1_700_000_000_500_000_000);
        assert!(meta.trace_ids.contains("aa"));

        let read = read_block(&dir.path().join(&meta.rel_path)).unwrap();
        assert_eq!(read, vec![a, b]); // exact, lossless round-trip
    }
}
