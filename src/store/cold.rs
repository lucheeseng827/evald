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
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::model::{Dialect, NormalizedSpan, Tokens};

/// What a freshly-written block contributes to the index at commit time.
pub struct BlockMeta {
    /// Path relative to the data dir (e.g. `blocks/2026/06/24/05/00..03-0.parquet`).
    pub rel_path: String,
    pub max_start_unix_nano: u64,
    pub trace_ids: BTreeSet<String>,
}

// --- block provenance (docs/FORMAT.md §"Block metadata") ------------------------------------
//
// Every block written by a format-1 engine carries these Parquet key-value pairs. They make a
// block self-describing to anything that mirrors the directory — the fleet uploader in
// particular, which must know which files a merged block replaced — without a sidecar file
// that could go missing separately. A block without them is a legacy (pre-marker) flush block,
// and its WAL seqno is recovered from its file name.

/// Key-value key: the on-disk format the writer followed (`"1"`).
pub const META_FORMAT: &str = "evald.format";
/// Key-value key: `flush` (one sealed WAL segment's spans for one partition) or `merged`.
pub const META_BLOCK_KIND: &str = "evald.block_kind";
/// Key-value key: the lowest WAL seqno whose spans this block holds.
pub const META_SEQNO_LO: &str = "evald.seqno_lo";
/// Key-value key: the highest WAL seqno whose spans this block holds.
pub const META_SEQNO_HI: &str = "evald.seqno_hi";
/// Key-value key: a JSON array of the data-dir-relative paths a merged block replaced.
pub const META_MERGED_FROM: &str = "evald.merged_from";
/// Key-value key: the evald version that wrote the block. Informational.
pub const META_WRITER: &str = "evald.writer";
/// Key-value key: a JSON array of the price-table versions whose derived costs the block may
/// hold — the table in force when a flush wrote it, the union of the inputs' for a merge. Absent
/// on blocks written before 0.3.0. Informational: the per-span truth is `evald cost --price-table`.
pub const META_PRICE_TABLES: &str = "evald.price_tables";

/// How a block came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// One sealed WAL segment's spans, for one hour partition — what compaction writes.
    Flush,
    /// Several blocks' spans, rewritten as one — what cold-to-cold merging writes.
    Merged,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Flush => "flush",
            BlockKind::Merged => "merged",
        }
    }
}

/// A block's provenance, as written into (or, for a legacy block, inferred about) it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub kind: BlockKind,
    pub seqno_lo: u64,
    pub seqno_hi: u64,
    /// Data-dir-relative paths of the blocks a merged block replaced. Empty for a flush.
    pub merged_from: Vec<String>,
    /// Price-table versions whose derived costs the block may hold (`META_PRICE_TABLES`);
    /// empty on a legacy block.
    pub price_tables: Vec<String>,
    /// True when read from the file's own metadata; false when inferred from a legacy
    /// (pre-marker) file name, which carries the seqno and nothing else.
    pub declared: bool,
}

impl Provenance {
    /// A flush block from WAL segment `seqno`.
    pub fn flush(seqno: u64) -> Self {
        Provenance {
            kind: BlockKind::Flush,
            seqno_lo: seqno,
            seqno_hi: seqno,
            merged_from: Vec::new(),
            price_tables: Vec::new(),
            declared: true,
        }
    }

    /// A merged block covering WAL seqnos `lo..=hi`, replacing `merged_from`.
    pub fn merged(seqno_lo: u64, seqno_hi: u64, merged_from: Vec<String>) -> Self {
        Provenance {
            kind: BlockKind::Merged,
            seqno_lo,
            seqno_hi,
            merged_from,
            price_tables: Vec::new(),
            declared: true,
        }
    }

    /// The price-table versions to record (a flush: the one in force; a merge: its inputs').
    pub fn with_price_tables(mut self, versions: Vec<String>) -> Self {
        self.price_tables = versions;
        self
    }

    /// The key-value pairs a block carrying this provenance is written with.
    pub fn key_value_metadata(&self) -> Vec<KeyValue> {
        let mut kv = vec![
            KeyValue::new(
                META_FORMAT.to_string(),
                super::format::FORMAT_VERSION.to_string(),
            ),
            KeyValue::new(META_BLOCK_KIND.to_string(), self.kind.as_str().to_string()),
            KeyValue::new(META_SEQNO_LO.to_string(), self.seqno_lo.to_string()),
            KeyValue::new(META_SEQNO_HI.to_string(), self.seqno_hi.to_string()),
            KeyValue::new(
                META_WRITER.to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
        ];
        if self.kind == BlockKind::Merged {
            kv.push(KeyValue::new(
                META_MERGED_FROM.to_string(),
                serde_json::to_string(&self.merged_from).unwrap_or_else(|_| "[]".into()),
            ));
        }
        if !self.price_tables.is_empty() {
            kv.push(KeyValue::new(
                META_PRICE_TABLES.to_string(),
                serde_json::to_string(&self.price_tables).unwrap_or_else(|_| "[]".into()),
            ));
        }
        kv
    }

    /// Decode from a file's key-value metadata, falling back to what the file name says for
    /// a legacy block. `stem` is the file name without `.parquet`.
    pub fn from_key_values(kv: Option<&Vec<KeyValue>>, stem: &str) -> Provenance {
        let get = |key: &str| {
            kv.and_then(|kv| kv.iter().find(|e| e.key == key))
                .and_then(|e| e.value.as_deref())
        };
        let kind = match get(META_BLOCK_KIND) {
            Some("merged") => Some(BlockKind::Merged),
            Some("flush") => Some(BlockKind::Flush),
            _ => None,
        };
        let declared = kind
            .zip(get(META_SEQNO_LO).zip(get(META_SEQNO_HI)))
            .and_then(|(kind, (lo, hi))| {
                Some((kind, lo.parse::<u64>().ok()?, hi.parse::<u64>().ok()?))
            });
        if let Some((kind, seqno_lo, seqno_hi)) = declared {
            let merged_from = get(META_MERGED_FROM)
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default();
            let price_tables = get(META_PRICE_TABLES)
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default();
            return Provenance {
                kind,
                seqno_lo,
                seqno_hi,
                merged_from,
                price_tables,
                declared: true,
            };
        }
        // Legacy: `<seqno:020>-<idx>` from the flush path, or a merged/day stem written by a
        // format-1 engine whose metadata could not be read — the seqno range is still in the
        // name.
        let (kind, lo, hi) = stem_range(stem);
        Provenance {
            kind,
            seqno_lo: lo,
            seqno_hi: hi,
            merged_from: Vec::new(),
            price_tables: Vec::new(),
            declared: false,
        }
    }
}

/// What a file name alone says: `<seqno:020>-<idx>` is a flush of that seqno;
/// `merged-<lo>-<hi>` and `day-<lo>-<hi>` cover a range. Anything else is seqno 0.
fn stem_range(stem: &str) -> (BlockKind, u64, u64) {
    let parse = |s: &str| s.parse::<u64>().ok();
    if let Some(rest) = stem
        .strip_prefix("merged-")
        .or_else(|| stem.strip_prefix("day-"))
    {
        let mut it = rest.split('-');
        if let (Some(lo), Some(hi)) = (it.next().and_then(parse), it.next().and_then(parse)) {
            return (BlockKind::Merged, lo, hi);
        }
    }
    let seqno = stem.split('-').next().and_then(parse).unwrap_or(0);
    (BlockKind::Flush, seqno, seqno)
}

/// The provenance and row count of a block, from its footer alone (no row data is read).
/// Reads only the Parquet footer, so this is cheap enough to call per block on a
/// maintenance pass (the merge planner and the fleet uploader both do).
pub fn read_provenance(abs_path: &Path) -> io::Result<(Provenance, u64)> {
    let stem = abs_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(abs_path)?)
        .map_err(io::Error::other)?;
    let file_meta = builder.metadata().file_metadata();
    let prov = Provenance::from_key_values(file_meta.key_value_metadata(), &stem);
    Ok((prov, file_meta.num_rows().max(0) as u64))
}

/// Writer properties every block is written with: Snappy, and the provenance metadata.
pub fn writer_properties(provenance: &Provenance) -> WriterProperties {
    writer_properties_builder(provenance).build()
}

/// The same, unfinished, for a writer that wants to add its own settings (row-group size).
pub fn writer_properties_builder(
    provenance: &Provenance,
) -> parquet::file::properties::WriterPropertiesBuilder {
    WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_key_value_metadata(Some(provenance.key_value_metadata()))
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
/// the index needs. `file_stem` must be unique within the partition; `provenance` is
/// written into the file's key-value metadata (docs/FORMAT.md).
pub fn write_block(
    data_dir: &Path,
    blocks_subdir: &str,
    partition: &str,
    file_stem: &str,
    spans: &[NormalizedSpan],
    provenance: &Provenance,
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
        let props = writer_properties(provenance);
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
pub(super) fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn fsync_dir(_dir: &Path) -> io::Result<()> {
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
            &Provenance::flush(7),
        )
        .unwrap();
        assert_eq!(meta.rel_path, "blocks/2023/11/14/22/0000-0.parquet");
        assert_eq!(meta.max_start_unix_nano, 1_700_000_000_500_000_000);
        assert!(meta.trace_ids.contains("aa"));

        let read = read_block(&dir.path().join(&meta.rel_path)).unwrap();
        assert_eq!(read, vec![a, b]); // exact, lossless round-trip
    }
}
