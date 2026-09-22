//! Cold-to-cold compaction — merging many small Parquet blocks into few large ones
//! (`CLOUD-ENTERPRISE-GAPS.md` C13a; `docs/FORMAT.md` §"Merged blocks").
//!
//! Why it exists: every sealed WAL segment became one block and nothing ever merged them, so
//! the block count only grew — and the read path's open-file working set grew with it until
//! `evald query` died with `EMFILE` (`docs/SOAK.md`). Merging bounds the count: an hour
//! partition collapses once it holds [`MergePolicy::hour_threshold`] blocks, a fully-past UTC
//! day collapses into day blocks, and every merged block is capped at
//! [`MergePolicy::max_spans_per_block`] rows so retention and memory stay granular.
//!
//! What it never changes: the commit protocol's shape. A merge writes its output the way a
//! flush does (temp → fsync → rename → fsync dir), then **one** redb transaction swaps the
//! inputs for the output — block table and trace mappings together — and only after that
//! commit is an input unreferenced. The inputs are not unlinked immediately: a query that
//! listed them a moment ago is still reading them, so they stay on disk for
//! [`MergePolicy::unlink_grace`] and are reclaimed by [`sweep_unindexed`], the same aged
//! orphan sweep that collects a crashed merge's output. The index is the source of truth
//! throughout: a file the index does not name is never read, and is eventually removed
//! whichever way it got there.
//!
//! Merged blocks are ordinary blocks — the same schema, readable by anything that reads the
//! flush blocks, dropped whole by retention on their newest span like any other. They differ
//! only in key-value metadata (`evald.block_kind = merged`, `evald.merged_from = [...]`),
//! which is how a consumer that mirrors this directory — the fleet uploader — learns which
//! files the merge replaced.
//!
//! **Write amplification is bounded by size tiering.** Merging every block in a partition
//! whenever it crosses the threshold would be quadratic: each pass re-reads everything the
//! previous passes wrote, so a partition under sustained ingest would rewrite its whole
//! history on every tick. So a tick merges only blocks of a **similar size** ([`tier`]):
//! sixteen blocks of one tier become one block several tiers higher, which is not an input
//! again until sixteen of ITS size exist. A span is therefore rewritten `O(log n)` times
//! over its life rather than once per pass. `evald compact` is explicitly the other thing —
//! an operator asking for a full collapse now — so it ignores tiers.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use arrow_array::{Array, RecordBatch, StringArray, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;

use super::cold::{self, BlockMeta, Provenance};
use super::index::Index;
use super::BLOCKS_SUBDIR;

/// Rows per Parquet row group in a merged block, and the batch size inputs are read in.
/// Bounds a merge's memory to roughly one row group of the widest columns, whatever the
/// output's size.
const ROW_GROUP_ROWS: usize = 65_536;

/// When cold blocks merge, and how large the result may be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePolicy {
    /// Merge an hour partition's blocks once this many of a **similar size** have
    /// accumulated (see the module docs on size tiering). `0` disables merging on the
    /// compactor's tick (`evald compact` still merges on demand).
    pub hour_threshold: usize,
    /// A merged block never exceeds this many spans; a partition with more is written as
    /// several. Keeps retention (which drops blocks whole) and merge memory granular.
    pub max_spans_per_block: usize,
    /// Also collapse each fully-past UTC day into day blocks, so a low-volume store's block
    /// count grows per day rather than per hour.
    pub day_merge: bool,
    /// On the compactor's tick, a closed day collapses only once nothing has been written
    /// into it for this long. A closed day that keeps receiving blocks — a backfill, a
    /// client with a skewed clock — would otherwise have its whole day block rewritten on
    /// every tick; while it is busy the hour threshold alone bounds its block count, and
    /// it collapses when the writes stop. `evald compact` ignores this and merges it now.
    pub day_quiet: Duration,
    /// How long a merge input stays on disk after it leaves the index, so a query that
    /// listed it moments before the merge can still read it.
    pub unlink_grace: Duration,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy {
            hour_threshold: 16,
            max_spans_per_block: 1_000_000,
            day_merge: true,
            day_quiet: Duration::from_secs(3600),
            unlink_grace: Duration::from_secs(60),
        }
    }
}

impl MergePolicy {
    /// Whether the compactor's tick merges at all.
    pub fn enabled(&self) -> bool {
        self.hour_threshold > 0
    }
}

/// How much a pass looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeScope {
    /// The compactor's tick: hour partitions at or over the threshold, closed days.
    Tick,
    /// `evald compact`: every hour partition with more than one block, plus closed days.
    Full,
}

/// What a pass did (or, on a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Merged blocks written.
    pub merges: usize,
    /// Input blocks consumed.
    pub blocks_in: usize,
    /// Output blocks written (equals `merges`; kept explicit for the report line).
    pub blocks_out: usize,
    /// Spans rewritten.
    pub spans: u64,
    /// Bytes of input Parquet consumed.
    pub bytes_in: u64,
    /// Bytes of output Parquet written (0 on a dry run).
    pub bytes_out: u64,
    pub dry_run: bool,
}

impl MergeReport {
    pub fn is_noop(&self) -> bool {
        self.merges == 0
    }
}

/// One block as the planner sees it.
struct Candidate {
    rel: String,
    abs: PathBuf,
    rows: u64,
    bytes: u64,
    /// When the file was last written — a flush or merge output's creation time, which is
    /// what "nothing written into this day lately" is measured from.
    modified: Option<SystemTime>,
    seqno_lo: u64,
    seqno_hi: u64,
    /// Carried through a merge: the output names every input's price tables.
    price_tables: Vec<String>,
}

/// Where a block sits in the time-partitioned tree.
enum Slot {
    /// `blocks/YYYY/MM/DD/HH/<file>` — a flush or an hour-merged block.
    Hour { hour: String, day: String },
    /// `blocks/YYYY/MM/DD/<file>` — a day-merged block.
    Day { day: String },
}

fn slot_of(rel: &str) -> Option<Slot> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.first() != Some(&BLOCKS_SUBDIR) || !rel.ends_with(".parquet") {
        return None;
    }
    let numeric = |s: &str| s.len() >= 2 && s.bytes().all(|b| b.is_ascii_digit());
    match parts.len() {
        6 if parts[1..5].iter().all(|p| numeric(p)) => Some(Slot::Hour {
            hour: parts[..5].join("/"),
            day: parts[..4].join("/"),
        }),
        5 if parts[1..4].iter().all(|p| numeric(p)) => Some(Slot::Day {
            day: parts[..4].join("/"),
        }),
        _ => None,
    }
}

/// `YYYY/MM/DD` for the current UTC day, in the partition tree's own spelling so a day
/// directory compares as a string.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .format("%Y/%m/%d")
        .to_string()
}

/// A day directory (`blocks/YYYY/MM/DD`) is closed once the UTC calendar has moved past it.
fn day_is_closed(day_dir: &str, today: &str) -> bool {
    day_dir.strip_prefix("blocks/").is_some_and(|d| d < today)
}

/// Blocks grouped by one level of the partition tree: directory → the blocks under it.
type Grouped = BTreeMap<String, Vec<String>>;

/// The index's blocks grouped by hour partition and by day.
fn group(index: &Index) -> io::Result<(Grouped, Grouped)> {
    let mut hours: Grouped = BTreeMap::new();
    let mut days: Grouped = BTreeMap::new();
    for (rel, _max_start) in index.blocks_with_max_start()? {
        match slot_of(&rel) {
            Some(Slot::Hour { hour, day }) => {
                hours.entry(hour).or_default().push(rel.clone());
                days.entry(day).or_default().push(rel);
            }
            Some(Slot::Day { day }) => days.entry(day).or_default().push(rel),
            // An unfamiliar layout is left exactly as it is: never merged, never deleted.
            None => {}
        }
    }
    Ok((hours, days))
}

/// Read what the planner needs from each block's footer. A block the index names but the
/// disk lacks is skipped with a warning rather than failing the pass: it is either a
/// crash-window artefact the open-time sweep will reconcile, or a bug worth the log line.
fn candidates(data_dir: &Path, rels: &[String]) -> io::Result<Vec<Candidate>> {
    let mut out = Vec::with_capacity(rels.len());
    for rel in rels {
        let abs = data_dir.join(rel);
        let (bytes, modified) = match fs::metadata(&abs) {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(%rel, "indexed block is missing on disk — skipping it for merging");
                continue;
            }
            Err(e) => return Err(e),
        };
        let (prov, rows) = cold::read_provenance(&abs)?;
        out.push(Candidate {
            rel: rel.clone(),
            abs,
            rows,
            bytes,
            modified,
            seqno_lo: prov.seqno_lo,
            seqno_hi: prov.seqno_hi,
            price_tables: prov.price_tables,
        });
    }
    Ok(out)
}

/// Whether nothing among `cands` was written within `quiet` of `now`. A block whose mtime
/// cannot be read counts as just written, so an unreadable timestamp defers a rewrite
/// rather than triggering one.
fn is_quiet(cands: &[Candidate], now: SystemTime, quiet: Duration) -> bool {
    cands.iter().all(|c| {
        c.modified
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= quiet)
    })
}

/// Which size class a block belongs to: `floor(log2(rows))`, so each tier holds blocks
/// within a factor of two of each other.
///
/// The property that matters: merging `hour_threshold` (≥ 2) blocks of tier `t` yields at
/// least `2 * 2^t` rows, so the output lands in a strictly higher tier and cannot be picked
/// up again by the same tier's next pass. That is what turns the quadratic rewrite into a
/// logarithmic one.
fn tier(rows: u64) -> u32 {
    rows.max(1).ilog2()
}

/// Greedy row-capped chunks in seqno order. A chunk of one block is not a merge and is
/// skipped by the caller; a single block over the cap is its own chunk and is never read again.
fn chunks(mut cands: Vec<Candidate>, max_spans: usize) -> Vec<Vec<Candidate>> {
    cands.sort_by(|a, b| (a.seqno_lo, &a.rel).cmp(&(b.seqno_lo, &b.rel)));
    let max = max_spans.max(1) as u64;
    let mut out: Vec<Vec<Candidate>> = Vec::new();
    let mut current: Vec<Candidate> = Vec::new();
    let mut rows: u64 = 0;
    for c in cands {
        if !current.is_empty() && rows.saturating_add(c.rows) > max {
            out.push(std::mem::take(&mut current));
            rows = 0;
        }
        rows = rows.saturating_add(c.rows);
        current.push(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One merge pass under the compaction lock. See the module docs for the protocol.
pub fn merge_cold(
    data_dir: &Path,
    index: &Index,
    compaction: &std::sync::Mutex<()>,
    policy: &MergePolicy,
    scope: MergeScope,
    dry_run: bool,
) -> io::Result<MergeReport> {
    let _guard = compaction.lock().unwrap_or_else(|p| p.into_inner());
    let mut report = MergeReport {
        dry_run,
        ..Default::default()
    };
    if scope == MergeScope::Tick && !policy.enabled() {
        return Ok(report);
    }
    // Blocks a planned-but-not-executed (dry-run) hour merge would consume, so the day phase
    // of a preview does not count them twice.
    let mut consumed: HashSet<String> = HashSet::new();

    // --- hour partitions ---------------------------------------------------------------
    let (hours, _) = group(index)?;
    for (hour_dir, rels) in hours {
        // Cheap gate before any footer is read: a partition that cannot possibly have a
        // full tier (or, for an explicit compact, a pair) is skipped without touching disk.
        let min_blocks = match scope {
            MergeScope::Tick => policy.hour_threshold,
            MergeScope::Full => 2,
        };
        if rels.len() < min_blocks {
            continue;
        }
        // On a tick, only blocks of a similar size merge together (see `tier`). An explicit
        // `evald compact` collapses the whole partition instead — the operator asked for it,
        // and it is one pass, not a loop.
        let groups: Vec<Vec<Candidate>> = match scope {
            MergeScope::Full => vec![candidates(data_dir, &rels)?],
            MergeScope::Tick => {
                let mut by_tier: BTreeMap<u32, Vec<Candidate>> = BTreeMap::new();
                for c in candidates(data_dir, &rels)? {
                    by_tier.entry(tier(c.rows)).or_default().push(c);
                }
                by_tier
                    .into_values()
                    .filter(|t| t.len() >= policy.hour_threshold)
                    .collect()
            }
        };
        for group in groups {
            for chunk in chunks(group, policy.max_spans_per_block) {
                if chunk.len() < 2 {
                    continue;
                }
                if dry_run {
                    consumed.extend(chunk.iter().map(|c| c.rel.clone()));
                }
                merge_chunk(
                    data_dir,
                    index,
                    &hour_dir,
                    "merged",
                    &chunk,
                    dry_run,
                    &mut report,
                )?;
            }
        }
    }

    // --- closed days --------------------------------------------------------------------
    if policy.day_merge {
        let today = today_utc();
        let now = SystemTime::now();
        // Re-grouped after the hour phase so a real run sees its merged hour blocks.
        let (_, days) = group(index)?;
        for (day_dir, rels) in days {
            if !day_is_closed(&day_dir, &today) {
                continue;
            }
            let rels: Vec<String> = rels.into_iter().filter(|r| !consumed.contains(r)).collect();
            if rels.len() < 2 {
                continue;
            }
            let cands = candidates(data_dir, &rels)?;
            if scope == MergeScope::Tick && !is_quiet(&cands, now, policy.day_quiet) {
                continue;
            }
            for chunk in chunks(cands, policy.max_spans_per_block) {
                if chunk.len() < 2 {
                    continue;
                }
                merge_chunk(
                    data_dir,
                    index,
                    &day_dir,
                    "day",
                    &chunk,
                    dry_run,
                    &mut report,
                )?;
            }
        }
    }
    Ok(report)
}

/// Rewrite `chunk` as one block under `out_dir_rel`, then commit the swap.
fn merge_chunk(
    data_dir: &Path,
    index: &Index,
    out_dir_rel: &str,
    prefix: &str,
    chunk: &[Candidate],
    dry_run: bool,
    report: &mut MergeReport,
) -> io::Result<()> {
    let seqno_lo = chunk.iter().map(|c| c.seqno_lo).min().unwrap_or(0);
    let seqno_hi = chunk.iter().map(|c| c.seqno_hi).max().unwrap_or(0);
    let stem = format!("{prefix}-{seqno_lo:020}-{seqno_hi:020}");
    let out_rel = format!("{out_dir_rel}/{stem}.parquet");
    if chunk.iter().any(|c| c.rel == out_rel) {
        // Only reachable if a merged block is re-merged with inputs inside its own range,
        // which the seqno-unique flush naming rules out; refuse rather than overwrite.
        tracing::warn!(%out_rel, "merge output would overwrite one of its inputs — skipping");
        return Ok(());
    }
    let rows_total: u64 = chunk.iter().map(|c| c.rows).sum();
    let bytes_in: u64 = chunk.iter().map(|c| c.bytes).sum();
    report.merges += 1;
    report.blocks_in += chunk.len();
    report.blocks_out += 1;
    report.spans += rows_total;
    report.bytes_in += bytes_in;
    if dry_run {
        return Ok(());
    }

    let abs_dir = data_dir.join(out_dir_rel);
    fs::create_dir_all(&abs_dir)?;
    let tmp_path = abs_dir.join(format!("{stem}.parquet.tmp"));
    let abs_out = data_dir.join(&out_rel);
    let merged_from: Vec<String> = chunk.iter().map(|c| c.rel.clone()).collect();
    let mut price_tables: Vec<String> = chunk
        .iter()
        .flat_map(|c| c.price_tables.iter().cloned())
        .collect();
    price_tables.sort();
    price_tables.dedup();
    let provenance =
        Provenance::merged(seqno_lo, seqno_hi, merged_from).with_price_tables(price_tables);

    let schema = cold::schema();
    let mut per_input: Vec<(String, BTreeSet<String>)> = Vec::with_capacity(chunk.len());
    let mut all_traces: BTreeSet<String> = BTreeSet::new();
    let mut max_start: u64 = 0;
    let mut rows_written: u64 = 0;
    let bytes_out = {
        let file = File::create(&tmp_path)?;
        let props = cold::writer_properties_builder(&provenance)
            .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
            .build();
        let mut writer =
            ArrowWriter::try_new(file, schema.clone(), Some(props)).map_err(io::Error::other)?;
        for c in chunk {
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&c.abs)?)
                .map_err(io::Error::other)?
                .with_batch_size(ROW_GROUP_ROWS)
                .build()
                .map_err(io::Error::other)?;
            let mut traces: BTreeSet<String> = BTreeSet::new();
            for batch in reader {
                let batch = batch.map_err(io::Error::other)?;
                // Re-home the batch on the canonical schema: this both drops per-file schema
                // metadata (which would otherwise differ block to block) and refuses a block
                // whose columns do not match the format — such a block is never rewritten.
                let batch = RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "block {} does not match the on-disk schema ({e}); refusing to merge it",
                                c.rel
                            ),
                        )
                    })?;
                collect_keys(&batch, &mut traces, &mut max_start)?;
                rows_written += batch.num_rows() as u64;
                writer.write(&batch).map_err(io::Error::other)?;
            }
            all_traces.extend(traces.iter().cloned());
            per_input.push((c.rel.clone(), traces));
        }
        let file = writer.into_inner().map_err(io::Error::other)?;
        file.sync_all()?;
        file.metadata()?.len()
    };
    fs::rename(&tmp_path, &abs_out)?;
    cold::fsync_dir(&abs_dir)?;

    let meta = BlockMeta {
        rel_path: out_rel.clone(),
        max_start_unix_nano: max_start,
        trace_ids: all_traces,
    };
    // Stamp the inputs' retirement time BEFORE the commit, not after. `sweep_unindexed`
    // measures the grace from mtime, so an input must already carry a fresh one at the
    // instant it stops being referenced. Doing it afterwards leaves a window — a crash, or a
    // failed stamp — in which the input is unreferenced while still carrying its original
    // mtime, and the very next sweep deletes it with no grace at all.
    //
    // This ordering has no such window. A failure here returns before `commit_merge`, so
    // nothing is retired and nothing can be swept early; the merge simply retries on the next
    // tick. A crash here leaves the inputs still indexed, and `sweep_unindexed` only ever
    // touches files the index does not name — their mtime is merely fresher than the truth,
    // which can only protect them for longer. The one visible effect of that case is that
    // `is_quiet` sees a recently-written partition and defers a day merge by the quiet
    // window, which is a delay, never a wrong answer.
    //
    // The output block is already renamed into place by now, so an abort here leaves it
    // unindexed on disk — exactly the case `sweep_unindexed` exists to reclaim.
    for (rel, _) in &per_input {
        mark_retired(&data_dir.join(rel)).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "could not stamp {rel} with its retirement time, so the unlink grace \
                     could not be honoured; merge abandoned rather than retiring it: {e}"
                ),
            )
        })?;
    }
    index.commit_merge(&meta, &per_input)?;
    report.bytes_out += bytes_out;
    // `debug`, not `info`: the compactor's tick logs one summary line per pass, and a
    // busy store merges continuously — two lines per merge is the store narrating itself.
    tracing::debug!(
        out = %out_rel,
        inputs = chunk.len(),
        rows = rows_written,
        bytes_in,
        bytes_out,
        "merged cold blocks"
    );
    Ok(())
}

/// Record that a block is about to leave the index, by setting its modification time to now.
///
/// `sweep_unindexed` reclaims an unreferenced file once it has been unreferenced for the
/// grace window, and the only per-file timestamp available without a second index is the
/// mtime. For a crashed flush's output or a stray temp file the write time *is* the moment
/// it became garbage, so mtime already means the right thing; for a merge input it does not,
/// which is what this call fixes.
///
/// The caller stamps before committing and treats a failure as fatal to that merge — see
/// [`merge_chunk`] for why that ordering leaves no window in which an input is unreferenced
/// but unstamped.
pub(crate) fn mark_retired(abs: &Path) -> io::Result<()> {
    let file = fs::File::options().write(true).open(abs)?;
    file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
}

/// Pull the trace ids and the newest start time out of a batch — the two things the index
/// records per block.
fn collect_keys(
    batch: &RecordBatch,
    traces: &mut BTreeSet<String>,
    max_start: &mut u64,
) -> io::Result<()> {
    let trace_col = batch
        .column_by_name("trace_id")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "block has no trace_id column")
        })?;
    let start_col = batch
        .column_by_name("start_unix_nano")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "block has no start_unix_nano column",
            )
        })?;
    for i in 0..batch.num_rows() {
        traces.insert(trace_col.value(i).to_string());
        if !start_col.is_null(i) {
            *max_start = (*max_start).max(start_col.value(i));
        }
    }
    Ok(())
}

/// Unlink every `*.parquet` under `blocks/` that the index does not name and every stray
/// `*.tmp`, once it has been unreferenced for `older_than`.
///
/// "Unreferenced for" is read from the file's mtime, which [`mark_retired`] sets when a
/// merge takes its inputs out of the index — so the window measures time since retirement,
/// not time since the block was written. Merge inputs past their grace,
/// and the output of a merge or flush that crashed before its commit. Then prune the
/// partition directories the unlinks emptied. Under the compaction lock, so it never
/// observes a flush between its rename and its commit.
pub fn sweep_unindexed(
    data_dir: &Path,
    index: &Index,
    compaction: &std::sync::Mutex<()>,
    older_than: Duration,
) -> io::Result<usize> {
    let _guard = compaction.lock().unwrap_or_else(|p| p.into_inner());
    let known: HashSet<String> = index.all_block_paths()?.into_iter().collect();
    let blocks_dir = data_dir.join(BLOCKS_SUBDIR);
    let now = SystemTime::now();
    let mut removed = 0usize;
    let mut stack = vec![blocks_dir.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let stray = if name.ends_with(".tmp") {
                true
            } else if name.ends_with(".parquet") {
                let rel = path
                    .strip_prefix(data_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                !known.contains(rel.as_str())
            } else {
                false
            };
            if !stray {
                continue;
            }
            let old_enough = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= older_than);
            if !old_enough {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    if removed > 0 {
        super::remove_empty_partition_dirs(&blocks_dir);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_parse_the_partition_tree_and_leave_the_unknown_alone() {
        assert!(matches!(
            slot_of("blocks/2026/09/16/10/00000000000000000042-0.parquet"),
            Some(Slot::Hour { ref hour, ref day })
                if hour == "blocks/2026/09/16/10" && day == "blocks/2026/09/16"
        ));
        assert!(matches!(
            slot_of("blocks/2026/09/16/day-00000000000000000001-00000000000000000042.parquet"),
            Some(Slot::Day { ref day }) if day == "blocks/2026/09/16"
        ));
        assert!(slot_of("blocks/2026/09/16/10/x.tmp").is_none());
        assert!(slot_of("scores/latest.parquet").is_none());
        assert!(slot_of("blocks/odd/layout/a.parquet").is_none());
        assert!(day_is_closed("blocks/2026/09/15", "2026/09/16"));
        assert!(!day_is_closed("blocks/2026/09/16", "2026/09/16"));
        assert!(!day_is_closed("blocks/2026/09/17", "2026/09/16"));
    }

    fn cand(rel: &str, rows: u64, seqno: u64) -> Candidate {
        Candidate {
            rel: rel.to_string(),
            abs: PathBuf::from(rel),
            rows,
            bytes: rows * 10,
            modified: None,
            seqno_lo: seqno,
            seqno_hi: seqno,
            price_tables: Vec::new(),
        }
    }

    #[test]
    fn quiet_means_every_block_is_older_than_the_window() {
        let now = SystemTime::now();
        let old = Candidate {
            modified: Some(now - Duration::from_secs(7_200)),
            ..cand("a", 1, 1)
        };
        let fresh = Candidate {
            modified: Some(now),
            ..cand("b", 1, 2)
        };
        let unknown = cand("c", 1, 3);
        let hour = Duration::from_secs(3_600);
        assert!(is_quiet(std::slice::from_ref(&old), now, hour));
        assert!(!is_quiet(std::slice::from_ref(&fresh), now, hour));
        assert!(is_quiet(std::slice::from_ref(&fresh), now, Duration::ZERO));
        assert!(!is_quiet(&[old, fresh], now, hour));
        // An unreadable mtime defers the rewrite rather than triggering it.
        assert!(!is_quiet(&[unknown], now, Duration::ZERO));
        assert!(is_quiet(&[], now, hour));
    }

    #[test]
    fn a_merged_block_always_lands_in_a_higher_tier_than_its_inputs() {
        // The property the amplification bound rests on: whatever size blocks merge, their
        // output is not an input to that same tier's next pass.
        for rows in [1u64, 20, 50_000, 999_999] {
            for threshold in [2usize, 16, 64] {
                let merged = rows * threshold as u64;
                assert!(
                    tier(merged) > tier(rows),
                    "{threshold} blocks of {rows} rows merged to {merged}: \
                     tier {} is not above tier {}",
                    tier(merged),
                    tier(rows)
                );
            }
        }
        // Blocks inside one `[2^k, 2^(k+1))` band share a tier, so the uniform flush blocks
        // a fixed seal threshold produces do meet each other. (A band boundary can still
        // fall between two similar sizes — 50k and 100k are a tier apart. That costs a
        // partition at most one extra unmerged group per tier, which is bounded, whereas
        // merging across all sizes is what was unbounded.)
        assert_eq!(tier(40_000), tier(60_000));
        assert_ne!(tier(60_000), tier(70_000));
        assert_eq!(
            tier(0),
            tier(1),
            "an empty block does not panic or get its own tier"
        );
    }

    #[test]
    fn chunks_are_row_capped_and_in_seqno_order() {
        let cands = vec![
            cand("c", 400, 3),
            cand("a", 400, 1),
            cand("b", 400, 2),
            cand("d", 1_500, 4),
            cand("e", 100, 5),
        ];
        let chunks = chunks(cands, 1_000);
        let rels: Vec<Vec<&str>> = chunks
            .iter()
            .map(|c| c.iter().map(|x| x.rel.as_str()).collect())
            .collect();
        // a+b fit (800); c would overflow → new chunk; d alone is over the cap and stays
        // alone; e starts a fresh chunk after it.
        assert_eq!(rels, vec![vec!["a", "b"], vec!["c"], vec!["d"], vec!["e"]]);
    }

    #[test]
    fn a_cap_of_zero_never_panics_and_still_chunks() {
        let chunks = chunks(vec![cand("a", 5, 1), cand("b", 5, 2)], 0);
        assert_eq!(chunks.len(), 2);
    }
}
