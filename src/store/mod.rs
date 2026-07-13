//! Durable store — PoC build step 4 (PLAN.md §1.2–1.3, §6).
//!
//! ```text
//!   handler --append--> bounded mpsc --> WRITER task ----------------+
//!                         | full=>429                 append+fsync   |
//!                         v                            active WAL seg |
//!                    (never drops)        seal at threshold ----------+--> sealed segs
//!                                                                          |
//!   COMPACTOR task (interval / on demand):                                v
//!     for each sealed seg > watermark, in order:                     hot tier
//!       1. write Parquet block(s) (tmp->fsync->rename->fsync dir)    (per-seg groups)
//!       2. ONE redb txn: record blocks + advance watermark           |
//!       3. delete the WAL segment + drop its hot group  <------------+
//!
//!   readers --query--> hot tier (un-flushed) ∪ cold (committed blocks via redb),
//!                      deduped by (trace_id, span_id)
//! ```
//!
//! The commit protocol (step 1→2→3 above) is crash-safe: the watermark only advances
//! after the Parquet block is fsynced and renamed into place, and the WAL segment is
//! only deleted after the watermark advance is durable. So on recovery, segments at or
//! below the watermark are already in cold (deleted), and segments above it are replayed
//! into the hot tier — no span is ever lost or double-counted. Orphan blocks from a
//! flush that crashed before its redb commit are swept on open (they were never indexed).
//!
//! **Backpressure.** The hot tier holds every un-compacted span in memory, so a compactor
//! that falls behind would grow it without bound. `StoreConfig::max_hot_spans` bounds it:
//! once the resident count reaches the limit, ingest is shed (`Backpressure` → `429 +
//! Retry-After`) instead of buffered in memory. Everything already accepted is durable in
//! the WAL, so this bounds *memory*, not durability — a hard shed threshold rather than an
//! OOM. `GET /v1/stats` exposes the backlog / shed counters so the approach is observable.

// Public on purpose: the cold tier is *plain Parquet* — an open format any engine can
// read — and that openness is a design guarantee (README "Plain Parquet out"). External
// tools (and the separately-licensed fleet sidecars, which depend one-way on this crate)
// consume the schema/reader here; the engine itself gains no awareness of them.
pub mod cold;
mod index;
mod scores;
mod wal;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::blob::BlobStore;
use crate::{NormalizedSpan, Score};
use index::Index;
use scores::ScoreStore;
use wal::Wal;

const WAL_SUBDIR: &str = "wal";
const BLOCKS_SUBDIR: &str = "blocks";
const BLOBS_SUBDIR: &str = "blobs";
const INDEX_FILE: &str = "index.redb";
const SCORES_FILE: &str = "scores.redb";

/// Why an `append` could not be accepted.
#[derive(Debug)]
pub enum StoreError {
    /// The ingest channel is full — shed load rather than drop or block unboundedly.
    Backpressure,
    /// The writer task is gone (shutting down).
    Closed,
    /// A WAL write/fsync failed.
    Io(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Backpressure => write!(f, "ingest overloaded (backpressure)"),
            StoreError::Closed => write!(f, "store writer is closed"),
            StoreError::Io(e) => write!(f, "WAL write failed: {e}"),
        }
    }
}
impl std::error::Error for StoreError {}

/// Tunables for [`Store::open`].
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Ingest channel depth before requests are shed with backpressure.
    pub channel_capacity: usize,
    /// Seal the active WAL segment once it holds this many spans (then it can compact).
    pub seal_threshold_spans: usize,
    /// How often the background compactor flushes sealed segments to cold. `None`
    /// disables background compaction (compact explicitly via [`Store::compact_now`]).
    pub compact_interval: Option<Duration>,
    /// Durable backlog bound: the max number of un-compacted spans allowed to accumulate
    /// in the in-memory hot tier before ingest is shed. Everything already accepted is
    /// durable in the WAL, so this bounds *memory* (the compactor drains the backlog),
    /// never durability — it is a "buffer to disk, don't OOM, shed only past a hard
    /// limit" backpressure policy. `0`
    /// disables the bound (unbounded hot tier — the pre-backpressure behavior).
    pub max_hot_spans: usize,
    /// Offload a span `input_value` / `output_value` larger than this many bytes to the blob
    /// store (leaving a compact `evald-blob:<key>` reference on the span), so a megabyte RAG
    /// context or tool output doesn't bloat the WAL / Parquet block / query response. `0`
    /// disables offloading (payloads stay inline).
    pub blob_offload_bytes: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            seal_threshold_spans: 50_000,
            compact_interval: Some(Duration::from_secs(5)),
            // ~1M un-compacted spans resident before shedding. At the 50k seal threshold
            // that is ~20 sealed segments of headroom for the compactor to catch up — large
            // enough that a healthy burst never trips it, small enough to bound memory.
            max_hot_spans: 1_000_000,
            // 256 KiB: well above a normal prompt/completion, below the RAG-context /
            // tool-output payloads that bloat the store and freeze the browser.
            blob_offload_bytes: 256 * 1024,
        }
    }
}

/// The outcome of a retention sweep ([`Store::reclaim_before`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimReport {
    /// The retention cutoff (span-start unix nanos); a block is dropped when its newest span
    /// predates this.
    pub cutoff_unix_nano: u64,
    /// Cold Parquet blocks dropped (or, on a dry run, that would be dropped).
    pub blocks_dropped: usize,
    /// Bytes of Parquet reclaimed — the summed on-disk size of the dropped blocks.
    pub bytes_reclaimed: u64,
    /// Blocks retained (newer than the cutoff).
    pub blocks_kept: usize,
    /// The oldest retained block's `max_start_unix_nano` — the next partition to age out of the
    /// window. `None` when nothing is retained.
    pub oldest_kept_unix_nano: Option<u64>,
    /// True when nothing was actually deleted (a preview).
    pub dry_run: bool,
}

/// A cloneable handle to the store.
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Cmd>,
    hot: Arc<RwLock<HotTier>>,
    index: Arc<Index>,
    scores: Arc<ScoreStore>,
    data_dir: Arc<PathBuf>,
    active_seqno: Arc<AtomicU64>,
    /// Serializes compaction passes so two never flush the same segment concurrently
    /// (which would race on the block's temp/final paths).
    compaction: Arc<std::sync::Mutex<()>>,
    /// Ingest channel depth (for the stats gauge) and the hot-tier bound / shed counter.
    channel_capacity: usize,
    max_hot_spans: usize,
    /// Cumulative spans shed because the hot tier hit `max_hot_spans` (durable-backlog
    /// backpressure, distinct from the transient channel-full shed).
    rejections: Arc<AtomicU64>,
    /// Externalized store for offloaded oversized payloads + the offload size cap.
    blobs: Arc<BlobStore>,
    blob_offload_bytes: usize,
}

/// A point-in-time snapshot of the ingest pipeline's load, for `GET /v1/stats` — the
/// queue-depth / backlog / rejection signal an operator needs to see the store approaching
/// its shed threshold *before* it starts returning 429s.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestStats {
    /// Un-compacted spans resident in the hot tier (the memory the compactor must drain).
    pub hot_spans: usize,
    /// The configured hot-tier bound (`0` = unbounded).
    pub max_hot_spans: usize,
    /// The ingest channel capacity (in-flight append commands before a channel-full shed).
    pub channel_capacity: usize,
    /// Cumulative spans shed because the hot tier was at its bound.
    pub rejections: u64,
    /// True once `hot_spans` is at/over the bound — ingest is currently shedding.
    pub shedding: bool,
}

/// The reply channel for an [`Cmd`]: `Ok` once the batch is fsynced, or a [`StoreError`]
/// (`Backpressure` when shed, `Io` on a WAL failure).
type Ack = oneshot::Sender<Result<(), StoreError>>;

enum Cmd {
    Append {
        spans: Vec<NormalizedSpan>,
        ack: Ack,
    },
    Seal {
        ack: Ack,
    },
}

/// A WAL-appended-but-not-yet-fsynced entry held during phase 1 of the writer's group
/// commit: its assigned segment seqno, the spans, and the caller's reply channel.
type PendingAppend = (u64, Vec<NormalizedSpan>, Ack);

impl Store {
    /// Open (or recover) the store under `data_dir`, then spawn the writer + compactor.
    /// Must run inside a Tokio runtime.
    pub fn open(data_dir: &Path, config: StoreConfig) -> io::Result<Store> {
        let wal_dir = data_dir.join(WAL_SUBDIR);
        let blocks_dir = data_dir.join(BLOCKS_SUBDIR);
        fs::create_dir_all(&wal_dir)?;
        fs::create_dir_all(&blocks_dir)?;

        let index = Arc::new(Index::open(&data_dir.join(INDEX_FILE))?);
        let scores = Arc::new(ScoreStore::open(&data_dir.join(SCORES_FILE))?);
        let blobs = Arc::new(BlobStore::open(data_dir.join(BLOBS_SUBDIR))?);
        let watermark = index.watermark()?;

        // Recovery: segments <= watermark are already in cold (delete them); segments
        // above it are replayed into the hot tier as their per-segment groups.
        let mut hot = HotTier::default();
        let mut max_seqno = watermark;
        for seqno in wal::list_segment_seqnos(&wal_dir)? {
            if seqno <= watermark {
                wal::delete_segment(&wal_dir, seqno)?;
            } else {
                hot.load_segment(seqno, wal::read_segment(&wal_dir, seqno)?);
                max_seqno = max_seqno.max(seqno);
            }
        }
        let active_seqno_val = max_seqno + 1;
        let recovered = hot.len();

        // Sweep Parquet files not referenced by the index (orphans from a crashed flush,
        // and leftover *.tmp). The index is the source of truth for committed blocks.
        sweep_orphans(data_dir, &blocks_dir, &index)?;

        let wal = Wal::open(&wal_dir, active_seqno_val)?;
        let hot = Arc::new(RwLock::new(hot));
        let active_seqno = Arc::new(AtomicU64::new(active_seqno_val));
        let data_dir = Arc::new(data_dir.to_path_buf());

        let compaction = Arc::new(std::sync::Mutex::new(()));
        let channel_capacity = config.channel_capacity.max(1);
        let rejections = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(channel_capacity);
        tokio::spawn(writer_loop(
            rx,
            wal,
            hot.clone(),
            active_seqno.clone(),
            config.seal_threshold_spans,
            config.max_hot_spans,
            rejections.clone(),
        ));
        if let Some(interval) = config.compact_interval {
            spawn_compactor(
                data_dir.clone(),
                index.clone(),
                hot.clone(),
                active_seqno.clone(),
                compaction.clone(),
                interval,
            );
        }

        tracing::info!(spans_recovered = recovered, watermark, "store opened");
        Ok(Store {
            tx,
            hot,
            index,
            scores,
            data_dir,
            active_seqno,
            compaction,
            channel_capacity,
            max_hot_spans: config.max_hot_spans,
            rejections,
            blobs,
            blob_offload_bytes: config.blob_offload_bytes,
        })
    }

    /// Offload any oversized `input_value` / `output_value` on these spans to the blob store,
    /// in place, replacing the field with an `evald-blob:<key>` reference. Called at ingest
    /// **before** [`Store::append`], so the WAL and Parquet blocks only ever carry the compact
    /// reference. Returns the number of fields offloaded. A no-op when offloading is disabled.
    pub fn offload_payloads(&self, spans: &mut [NormalizedSpan]) -> usize {
        crate::blob::offload_large_payloads(spans, &self.blobs, self.blob_offload_bytes)
    }

    /// Fetch an offloaded payload by its blob key (or full `evald-blob:<key>` reference).
    /// `Ok(None)` for an unknown / malformed key.
    pub fn get_blob(&self, key_or_ref: &str) -> io::Result<Option<Vec<u8>>> {
        self.blobs.get(key_or_ref)
    }

    /// A snapshot of ingest load (backlog / channel depth / cumulative sheds) for the stats
    /// endpoint. Cheap: one hot-tier read lock + two atomic loads.
    pub fn ingest_stats(&self) -> IngestStats {
        let hot_spans = self.hot.read().expect("hot tier lock").len();
        let shedding = self.max_hot_spans != 0 && hot_spans >= self.max_hot_spans;
        IngestStats {
            hot_spans,
            max_hot_spans: self.max_hot_spans,
            channel_capacity: self.channel_capacity,
            rejections: self.rejections.load(Ordering::Relaxed),
            shedding,
        }
    }

    /// Durably append a batch of spans (returns once fsynced to the WAL), or a
    /// [`StoreError`] if shed or failed. An empty batch is a no-op.
    pub async fn append(&self, spans: Vec<NormalizedSpan>) -> Result<(), StoreError> {
        let (ack, ack_rx) = oneshot::channel();
        self.tx
            .try_send(Cmd::Append { spans, ack })
            .map_err(map_try_send)?;
        recv_ack(ack_rx).await
    }

    /// Seal the active WAL segment so it becomes eligible for compaction (no-op if the
    /// active segment is empty). Mainly for tests / explicit flush points.
    pub async fn seal_now(&self) -> Result<(), StoreError> {
        let (ack, ack_rx) = oneshot::channel();
        self.tx.try_send(Cmd::Seal { ack }).map_err(map_try_send)?;
        recv_ack(ack_rx).await
    }

    /// Run one compaction pass now (flush all sealed segments), synchronously.
    pub async fn compact_now(&self) -> io::Result<()> {
        let data_dir = self.data_dir.clone();
        let index = self.index.clone();
        let hot = self.hot.clone();
        let active_seqno = self.active_seqno.clone();
        let compaction = self.compaction.clone();
        tokio::task::spawn_blocking(move || {
            flush_pending(&data_dir, &index, &hot, &active_seqno, &compaction)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Most-recent-first spans (hot ∪ cold, deduped), optionally filtered by `trace_id`.
    pub fn query(&self, trace_id: Option<&str>, limit: usize) -> io::Result<Vec<NormalizedSpan>> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut out: Vec<NormalizedSpan> = Vec::new();

        // Hot first — un-flushed, freshest.
        {
            let hot = self.hot.read().expect("hot tier lock");
            for s in hot.all_spans() {
                if trace_matches(trace_id, s) && seen.insert(span_key(s)) {
                    out.push(s.clone());
                }
            }
        }
        // Cold — only blocks the index says are committed (never a raw dir scan).
        let block_paths = match trace_id {
            Some(t) => self.index.block_paths_for_trace(t)?,
            None => self.index.all_block_paths()?,
        };
        for rel in block_paths {
            for s in cold::read_block(&self.data_dir.join(&rel))? {
                if trace_matches(trace_id, &s) && seen.insert(span_key(&s)) {
                    out.push(s);
                }
            }
        }

        out.sort_by_key(|b| std::cmp::Reverse(b.start_unix_nano));
        out.truncate(limit);
        Ok(out)
    }

    /// All spans of one trace (hot ∪ cold, deduped), in arrival (chronological) order.
    pub fn trace(&self, trace_id: &str) -> io::Result<Vec<NormalizedSpan>> {
        let mut spans = self.query(Some(trace_id), usize::MAX)?;
        spans.sort_by_key(|s| s.start_unix_nano);
        Ok(spans)
    }

    /// Total distinct spans across hot + cold.
    pub fn span_count(&self) -> io::Result<usize> {
        Ok(self.query(None, usize::MAX)?.len())
    }

    // --- scores (PLAN.md §2.2) ---------------------------------------------------

    /// Durably upsert a batch of scores.
    pub fn put_scores(&self, scores: &[Score]) -> io::Result<()> {
        self.scores.put_batch(scores)
    }

    /// A single score by id.
    pub fn get_score(&self, id: &str) -> io::Result<Option<Score>> {
        self.scores.get(id)
    }

    /// All scores for a target (e.g. a span or trace), newest-first.
    pub fn scores_for_target(&self, target: &crate::ScoreTarget) -> io::Result<Vec<Score>> {
        self.scores.by_target_key(&target.key())
    }

    /// Recent scores across all targets, newest-first, capped at `limit`.
    pub fn list_scores(&self, limit: usize) -> io::Result<Vec<Score>> {
        self.scores.list(limit)
    }

    /// Total scores stored.
    pub fn score_count(&self) -> io::Result<usize> {
        self.scores.count()
    }

    // --- query-engine support (PoC step 8) ---------------------------------------

    /// The store's data directory (root of `wal/`, `blocks/`, the redb files).
    pub fn data_dir(&self) -> &Path {
        self.data_dir.as_path()
    }

    /// The cold-block directory (`<data_dir>/blocks`) — the time-partitioned Parquet under
    /// `blocks/YYYY/MM/DD/HH/*.parquet`.
    pub fn blocks_dir(&self) -> PathBuf {
        self.data_dir.join(BLOCKS_SUBDIR)
    }

    /// Absolute paths of the **committed** cold blocks, per the redb index — never a raw dir
    /// scan, so an orphan block from a crashed flush (written but not yet indexed) is excluded,
    /// exactly like [`Store::query`]. The SQL engine registers these explicitly as its
    /// `cold_spans` table (a directory/glob `ListingTable` both misses the nested partition
    /// tree and could surface un-committed orphans).
    pub fn cold_block_paths(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self
            .index
            .all_block_paths()?
            .into_iter()
            .map(|rel| self.data_dir.join(rel))
            .collect())
    }

    /// A snapshot of the hot tier — un-compacted spans still living in memory / the WAL,
    /// not yet flushed to a Parquet block. The SQL engine unions these with the cold
    /// blocks (deduping by `(trace_id, span_id)`) so a query sees the full, current set.
    pub fn hot_spans(&self) -> Vec<NormalizedSpan> {
        let hot = self.hot.read().expect("hot tier lock");
        hot.all_spans().cloned().collect()
    }

    /// Retention by **partition drop**: unlink every cold Parquet block whose spans *all* started
    /// before `cutoff_unix_nano`, reclaiming its disk whole. The index entry (block + its
    /// `trace_id → block` mappings) is removed first and then the file is unlinked; a file left
    /// behind by a crash mid-sweep is an orphan the open-time sweep collects, so this is crash-safe.
    /// Retention is therefore `O(unlink)` and space-reclaiming by construction — no row-by-row
    /// `DELETE`, no vacuum, no full-table scan (the OOM-on-delete / disk-not-reclaimed
    /// class of bug is designed out). The hot tier (un-flushed spans) is never touched. `dry_run`
    /// computes the same report without deleting anything. Runs under the compaction lock so it can
    /// never race a flush.
    pub fn reclaim_before(
        &self,
        cutoff_unix_nano: u64,
        dry_run: bool,
    ) -> io::Result<ReclaimReport> {
        let _guard = self.compaction.lock().expect("compaction lock");

        let mut drop_paths: Vec<String> = Vec::new();
        let mut bytes_reclaimed: u64 = 0;
        let mut blocks_kept = 0usize;
        let mut oldest_kept: Option<u64> = None;
        for (rel, max_start) in self.index.blocks_with_max_start()? {
            if max_start < cutoff_unix_nano {
                // The block's newest span predates the cutoff → every span in it is expired.
                if let Ok(meta) = fs::metadata(self.data_dir.join(&rel)) {
                    bytes_reclaimed = bytes_reclaimed.saturating_add(meta.len());
                }
                drop_paths.push(rel);
            } else {
                blocks_kept += 1;
                oldest_kept = Some(oldest_kept.map_or(max_start, |o| o.min(max_start)));
            }
        }
        let blocks_dropped = drop_paths.len();

        if !dry_run && !drop_paths.is_empty() {
            // Index first (crash-safe: a leftover file becomes a swept orphan), then unlink.
            self.index.drop_blocks(&drop_paths)?;
            for rel in &drop_paths {
                match fs::remove_file(self.data_dir.join(rel)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            remove_empty_partition_dirs(&self.data_dir.join(BLOCKS_SUBDIR));
        }

        Ok(ReclaimReport {
            cutoff_unix_nano,
            blocks_dropped,
            bytes_reclaimed,
            blocks_kept,
            oldest_kept_unix_nano: oldest_kept,
            dry_run,
        })
    }
}

/// Best-effort removal of now-empty `YYYY/MM/DD/HH` partition directories under `blocks_dir` after
/// a retention sweep unlinked their blocks. Never fails the sweep — a directory that isn't empty
/// (or can't be removed) is simply left. `blocks_dir` itself is always kept.
fn remove_empty_partition_dirs(blocks_dir: &Path) {
    /// Returns true if `dir` ended up empty (and was removed), so the caller can cascade upward.
    fn prune(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut empty = true;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !prune(&path) {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        if empty {
            empty = fs::remove_dir(dir).is_ok();
        }
        empty
    }
    let Ok(entries) = fs::read_dir(blocks_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune(&path);
        }
    }
}

fn map_try_send<T>(e: mpsc::error::TrySendError<T>) -> StoreError {
    match e {
        mpsc::error::TrySendError::Full(_) => StoreError::Backpressure,
        mpsc::error::TrySendError::Closed(_) => StoreError::Closed,
    }
}

async fn recv_ack(ack_rx: oneshot::Receiver<Result<(), StoreError>>) -> Result<(), StoreError> {
    match ack_rx.await {
        Ok(res) => res,
        Err(_) => Err(StoreError::Closed),
    }
}

fn span_key(s: &NormalizedSpan) -> (String, String) {
    (s.trace_id.clone(), s.span_id.clone())
}

fn trace_matches(filter: Option<&str>, s: &NormalizedSpan) -> bool {
    match filter {
        Some(t) => s.trace_id == t,
        None => true,
    }
}

/// The single writer task: append+fsush to the active WAL segment, publish to the hot
/// tier, ACK, and seal the segment once it crosses the threshold.
#[allow(clippy::too_many_arguments)]
async fn writer_loop(
    mut rx: mpsc::Receiver<Cmd>,
    mut wal: Wal,
    hot: Arc<RwLock<HotTier>>,
    active_seqno: Arc<AtomicU64>,
    seal_threshold: usize,
    max_hot_spans: usize,
    rejections: Arc<AtomicU64>,
) {
    // Group commit: after the blocking recv, drain whatever else is already queued (up
    // to a cap) so all waiting appends share ONE fsync. Every caller is still ACKed
    // only after that fsync — the durability guarantee is unchanged; only the cost is
    // amortized. Under a single slow client nothing batches (identical behavior); under
    // concurrent load the fsync stops being a per-request tax.
    const GROUP_COMMIT_MAX: usize = 128;
    while let Some(first) = rx.recv().await {
        let mut cmds = vec![first];
        while cmds.len() < GROUP_COMMIT_MAX {
            // Stop draining at a Seal so append/seal ordering is preserved.
            if matches!(cmds.last(), Some(Cmd::Seal { .. })) {
                break;
            }
            match rx.try_recv() {
                Ok(cmd) => cmds.push(cmd),
                Err(_) => break,
            }
        }

        // Durable-backlog backpressure: the hot tier holds every un-compacted span in
        // memory, so if the compactor falls behind it would grow without bound (an OOM). The
        // bound is a soft, per-group gate — everything already accepted is safe in the WAL,
        // so once we are at the limit we *shed the new batch* (429 + Retry-After at the HTTP
        // edge) rather than buffer it in memory. Checked once per group (the hot tier only
        // grows in phase 2), so a single big first batch may overshoot the bound once.
        let over_limit =
            max_hot_spans != 0 && hot.read().expect("hot tier lock").len() >= max_hot_spans;

        // Phase 1: buffered appends (no fsync yet). Individual encode/write failures
        // are ACKed Err immediately and excluded from the commit.
        let mut pending: Vec<PendingAppend> = Vec::with_capacity(cmds.len());
        let mut seal_ack = None;
        for cmd in cmds {
            match cmd {
                Cmd::Append { spans, ack } => {
                    if over_limit {
                        rejections.fetch_add(spans.len() as u64, Ordering::Relaxed);
                        let _ = ack.send(Err(StoreError::Backpressure));
                        continue;
                    }
                    match wal.append_nosync(&spans) {
                        Ok(seqno) => pending.push((seqno, spans, ack)),
                        Err(e) => {
                            let _ = ack.send(Err(StoreError::Io(e)));
                        }
                    }
                }
                Cmd::Seal { ack } => seal_ack = Some(ack),
            }
        }

        // Phase 2: one fsync commits the whole group, then ACK everyone.
        if !pending.is_empty() {
            match wal.sync() {
                Ok(()) => {
                    for (seqno, spans, ack) in pending {
                        // Insert-then-ACK: the hot push is an in-memory op, and
                        // compaction runs on a *separate* task, so the append path
                        // never stalls on it.
                        hot.write().expect("hot tier lock").push(seqno, spans);
                        let _ = ack.send(Ok(()));
                    }
                }
                Err(e) => {
                    for (_, _, ack) in pending {
                        let _ =
                            ack.send(Err(StoreError::Io(io::Error::new(e.kind(), e.to_string()))));
                    }
                }
            }
            if wal.active_count() >= seal_threshold {
                if let Err(e) = seal(&mut wal, &active_seqno) {
                    tracing::error!(%e, "failed to seal WAL segment");
                }
            }
        }

        if let Some(ack) = seal_ack {
            let result = if wal.active_count() == 0 {
                Ok(()) // nothing to seal
            } else {
                seal(&mut wal, &active_seqno)
                    .map(|_| ())
                    .map_err(StoreError::Io)
            };
            let _ = ack.send(result);
        }
    }
}

fn seal(wal: &mut Wal, active_seqno: &AtomicU64) -> io::Result<u64> {
    let sealed = wal.seal_and_rotate()?;
    active_seqno.store(wal.active_seqno(), Ordering::SeqCst);
    Ok(sealed)
}

/// Spawn the background compactor: every `interval`, flush sealed segments to cold.
fn spawn_compactor(
    data_dir: Arc<PathBuf>,
    index: Arc<Index>,
    hot: Arc<RwLock<HotTier>>,
    active_seqno: Arc<AtomicU64>,
    compaction: Arc<std::sync::Mutex<()>>,
    interval: Duration,
) {
    tokio::spawn(async move {
        // Start one interval out, not immediately, so opening the store doesn't kick a
        // compaction pass that races an explicit `compact_now`.
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let (data_dir, index, hot, active_seqno, compaction) = (
                data_dir.clone(),
                index.clone(),
                hot.clone(),
                active_seqno.clone(),
                compaction.clone(),
            );
            let result = tokio::task::spawn_blocking(move || {
                flush_pending(&data_dir, &index, &hot, &active_seqno, &compaction)
            })
            .await;
            if let Ok(Err(e)) = result {
                tracing::error!(%e, "compaction pass failed (will retry)");
            }
        }
    });
}

/// Flush every sealed segment (watermark < seqno < active) to cold, in order. The
/// `compaction` mutex makes passes mutually exclusive, so two never flush the same
/// segment concurrently (which would race on the block's temp/final paths).
fn flush_pending(
    data_dir: &Path,
    index: &Index,
    hot: &RwLock<HotTier>,
    active_seqno: &AtomicU64,
    compaction: &std::sync::Mutex<()>,
) -> io::Result<()> {
    let _guard = compaction.lock().unwrap_or_else(|p| p.into_inner());
    let wal_dir = data_dir.join(WAL_SUBDIR);
    let active = active_seqno.load(Ordering::SeqCst);
    let watermark = index.watermark()?;
    let pending: Vec<u64> = hot
        .read()
        .expect("hot tier lock")
        .segnos()
        .into_iter()
        .filter(|s| *s > watermark && *s < active)
        .collect();

    for seqno in pending {
        flush_one(data_dir, &wal_dir, index, hot, seqno)?;
    }
    Ok(())
}

/// Flush one sealed segment, following the commit protocol exactly.
fn flush_one(
    data_dir: &Path,
    wal_dir: &Path,
    index: &Index,
    hot: &RwLock<HotTier>,
    seqno: u64,
) -> io::Result<()> {
    // Snapshot the segment's spans without holding the lock during the Parquet write.
    let spans = match hot.read().expect("hot tier lock").segment(seqno) {
        Some(spans) => spans.clone(),
        None => return Ok(()),
    };

    // 1. Write block(s) durably (tmp -> fsync -> rename -> fsync dir), one per partition.
    let mut blocks = Vec::new();
    if !spans.is_empty() {
        let mut by_partition: BTreeMap<String, Vec<NormalizedSpan>> = BTreeMap::new();
        for s in spans {
            by_partition
                .entry(cold::partition_of(s.start_unix_nano))
                .or_default()
                .push(s);
        }
        for (idx, (partition, group)) in by_partition.into_iter().enumerate() {
            let stem = format!("{seqno:020}-{idx}");
            blocks.push(cold::write_block(
                data_dir,
                BLOCKS_SUBDIR,
                &partition,
                &stem,
                &group,
            )?);
        }
    }

    // 2. Commit: record blocks + advance the watermark to `seqno`, atomically + durably.
    index.commit_flush(seqno, &blocks)?;

    // 3. Only now truncate the WAL (delete the segment) and drop the hot group.
    wal::delete_segment(wal_dir, seqno)?;
    hot.write().expect("hot tier lock").drop_segment(seqno);
    tracing::info!(
        seqno,
        blocks = blocks.len(),
        "compacted WAL segment to cold"
    );
    Ok(())
}

/// Delete Parquet files (and stale `*.tmp`) under `blocks/` that the index does not
/// reference — orphans from a flush that crashed before its redb commit.
fn sweep_orphans(data_dir: &Path, blocks_dir: &Path, index: &Index) -> io::Result<()> {
    let known: HashSet<String> = index.all_block_paths()?.into_iter().collect();
    let mut stack = vec![blocks_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".tmp") {
                fs::remove_file(&path)?;
            } else if name.ends_with(".parquet") {
                let rel = path
                    .strip_prefix(data_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !known.contains(rel.as_str()) {
                    tracing::warn!(%rel, "removing orphan parquet block (not in index)");
                    fs::remove_file(&path)?;
                }
            }
        }
    }
    Ok(())
}

/// In-memory hot tier: un-flushed spans grouped by their WAL segment, so a flushed
/// segment can be dropped wholesale once it is durable in cold.
#[derive(Default)]
struct HotTier {
    segments: BTreeMap<u64, Vec<NormalizedSpan>>,
}

impl HotTier {
    fn load_segment(&mut self, seqno: u64, spans: Vec<NormalizedSpan>) {
        if !spans.is_empty() {
            self.segments.insert(seqno, spans);
        }
    }
    fn push(&mut self, seqno: u64, spans: Vec<NormalizedSpan>) {
        self.segments.entry(seqno).or_default().extend(spans);
    }
    fn drop_segment(&mut self, seqno: u64) {
        self.segments.remove(&seqno);
    }
    fn segment(&self, seqno: u64) -> Option<&Vec<NormalizedSpan>> {
        self.segments.get(&seqno)
    }
    fn segnos(&self) -> Vec<u64> {
        self.segments.keys().copied().collect()
    }
    fn all_spans(&self) -> impl Iterator<Item = &NormalizedSpan> {
        self.segments.values().flatten()
    }
    fn len(&self) -> usize {
        self.segments.values().map(|v| v.len()).sum()
    }
}

#[cfg(test)]
pub(crate) fn test_span(trace_id: &str, span_id: &str, start_unix_nano: u64) -> NormalizedSpan {
    use crate::model::{Dialect, Tokens};
    NormalizedSpan {
        dialect: Dialect::Unknown,
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        name: "span".to_string(),
        otel_kind: 0,
        oi_kind: None,
        start_unix_nano,
        end_unix_nano: start_unix_nano + 1,
        status_code: 0,
        status_message: None,
        model: None,
        provider: None,
        tokens: Tokens::default(),
        cost_usd: None,
        input_value: None,
        output_value: None,
        session_id: None,
        user_id: None,
        service_name: None,
        scope_name: None,
        scope_version: None,
        raw_attributes: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> StoreConfig {
        StoreConfig {
            channel_capacity: 16,
            seal_threshold_spans: 100_000, // never auto-seal in tests; we seal explicitly
            compact_interval: None,        // no background compactor; tests call compact_now
            max_hot_spans: 0,              // unbounded hot tier; backpressure has its own test
            blob_offload_bytes: 0,         // no offload; blob storage has its own test
        }
    }

    async fn open(dir: &Path) -> Store {
        Store::open(dir, test_config()).unwrap()
    }

    #[tokio::test]
    async fn hot_tier_bound_sheds_past_the_limit_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny hot bound, no compactor → the hot tier can never drain, so once it fills the
        // next append is shed (durable-backlog backpressure) rather than growing memory.
        let config = StoreConfig {
            channel_capacity: 16,
            seal_threshold_spans: 100_000,
            compact_interval: None,
            max_hot_spans: 3,
            blob_offload_bytes: 0,
        };
        let store = Store::open(dir.path(), config).unwrap();

        // Fill to the bound — each of these is accepted (hot len < 3 at check time), so
        // exactly `max_hot_spans` spans end up resident.
        for i in 0..3u64 {
            store
                .append(vec![test_span("aa", &format!("{i:02}"), 1_000 + i)])
                .await
                .unwrap();
        }
        // At the bound: nothing shed yet, but `shedding` flags that the next append will.
        assert!(store.ingest_stats().shedding, "hot tier is at its bound");
        assert_eq!(store.ingest_stats().rejections, 0, "nothing shed yet");

        // Hot tier is now AT the bound → the next append sheds with Backpressure.
        let err = store
            .append(vec![test_span("aa", "99", 9_999)])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Backpressure), "got {err:?}");

        let stats = store.ingest_stats();
        assert_eq!(stats.hot_spans, 3);
        assert_eq!(stats.max_hot_spans, 3);
        assert!(stats.shedding);
        assert_eq!(stats.rejections, 1, "one span was shed");
        // The shed span was never persisted — no silent drop of accepted data, no over-count.
        assert_eq!(store.span_count().unwrap(), 3);
    }

    #[tokio::test]
    async fn unbounded_hot_tier_never_sheds() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await; // test_config → max_hot_spans: 0 (unbounded)
        for i in 0..50u64 {
            store
                .append(vec![test_span("aa", &format!("{i:02}"), 1_000 + i)])
                .await
                .unwrap();
        }
        let stats = store.ingest_stats();
        assert_eq!(stats.max_hot_spans, 0);
        assert!(!stats.shedding);
        assert_eq!(stats.rejections, 0);
        assert_eq!(store.span_count().unwrap(), 50);
    }

    #[tokio::test]
    async fn compaction_flushes_to_cold_and_truncates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
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

        // WAL: the sealed segment (seqno 1) is gone; only the empty active (2) remains.
        let segs = wal::list_segment_seqnos(&dir.path().join(WAL_SUBDIR)).unwrap();
        assert_eq!(segs, vec![2]);
        // index watermark advanced; a parquet block exists.
        assert_eq!(store.index.watermark().unwrap(), 1);
        assert_eq!(store.index.all_block_paths().unwrap().len(), 1);
        // hot tier emptied; data is now served from cold.
        assert_eq!(store.hot.read().unwrap().len(), 0);
        assert_eq!(store.span_count().unwrap(), 2);
        assert_eq!(store.query(None, 10).unwrap()[0].span_id, "02"); // newest-first
        assert_eq!(store.trace("aa").unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reclaim_before_drops_old_blocks_and_reclaims_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        const OLD: u64 = 1_600_000_000_000_000_000; // 2020 → an old hour-partition
        const NEW: u64 = 1_800_000_000_000_000_000; // 2027 → a different, recent partition
                                                    // Two separate cold blocks — compact between the appends so each seals its own segment.
        store
            .append(vec![test_span("old", "01", OLD)])
            .await
            .unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        store
            .append(vec![test_span("new", "02", NEW)])
            .await
            .unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        assert_eq!(store.index.all_block_paths().unwrap().len(), 2);
        assert_eq!(store.span_count().unwrap(), 2);

        const CUTOFF: u64 = 1_700_000_000_000_000_000; // between OLD and NEW

        // Dry run: reports the drop but touches nothing.
        let preview = store.reclaim_before(CUTOFF, true).unwrap();
        assert_eq!(preview.blocks_dropped, 1);
        assert_eq!(preview.blocks_kept, 1);
        assert!(preview.dry_run);
        assert!(preview.bytes_reclaimed > 0);
        assert_eq!(store.index.all_block_paths().unwrap().len(), 2);
        assert_eq!(store.span_count().unwrap(), 2);

        // Apply: the old block + its trace mapping are gone; disk reclaimed; the recent span stays.
        let report = store.reclaim_before(CUTOFF, false).unwrap();
        assert_eq!(report.blocks_dropped, 1);
        assert_eq!(report.blocks_kept, 1);
        assert!(report.bytes_reclaimed > 0);
        assert!(!report.dry_run);
        assert_eq!(report.oldest_kept_unix_nano, Some(NEW));
        assert_eq!(store.index.all_block_paths().unwrap().len(), 1);
        assert_eq!(store.span_count().unwrap(), 1);
        assert!(store.trace("old").unwrap().is_empty());
        assert_eq!(store.trace("new").unwrap().len(), 1);

        // Idempotent: a second sweep at the same cutoff drops nothing more.
        let again = store.reclaim_before(CUTOFF, false).unwrap();
        assert_eq!(again.blocks_dropped, 0);
        assert_eq!(again.blocks_kept, 1);

        // Reopen: no orphan Parquet resurrects the dropped span (index is the source of truth).
        drop(store);
        let store = open(dir.path()).await;
        assert_eq!(store.span_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn query_unions_hot_and_cold_without_dupes() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;
        // batch A -> cold
        store.append(vec![test_span("aa", "01", 10)]).await.unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        // batch B -> hot (un-flushed)
        store.append(vec![test_span("aa", "02", 20)]).await.unwrap();

        assert_eq!(store.hot.read().unwrap().len(), 1);
        let all = store.query(None, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].span_id, "02"); // newest first (hot)
        assert_eq!(all[1].span_id, "01"); // cold
        assert_eq!(store.trace("aa").unwrap().len(), 2);
        // a no-op extra compaction must not double-count
        store.compact_now().await.unwrap();
        assert_eq!(store.span_count().unwrap(), 2);
    }

    #[tokio::test]
    async fn survives_reopen_with_cold_and_hot() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path()).await;
            store.append(vec![test_span("aa", "01", 10)]).await.unwrap(); // -> cold
            store.seal_now().await.unwrap();
            store.compact_now().await.unwrap();
            store.append(vec![test_span("bb", "02", 20)]).await.unwrap(); // -> hot
            assert_eq!(store.span_count().unwrap(), 2);
        }
        // Reopen: cold survives via the index/watermark, hot survives via WAL replay.
        let store = open(dir.path()).await;
        assert_eq!(store.span_count().unwrap(), 2);
        assert_eq!(store.trace("aa").unwrap().len(), 1); // from cold
        assert_eq!(store.trace("bb").unwrap().len(), 1); // replayed into hot
    }

    #[tokio::test]
    async fn flushed_segment_left_on_disk_is_not_double_counted() {
        // Simulates a crash AFTER the watermark advanced but BEFORE the WAL segment was
        // deleted: recovery must treat seqno <= watermark as already-in-cold.
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path()).await;
            store.append(vec![test_span("aa", "01", 10)]).await.unwrap();
            store.seal_now().await.unwrap();
            store.compact_now().await.unwrap();
            assert_eq!(store.index.watermark().unwrap(), 1);
        }
        // Re-create the already-flushed segment file (as if the delete hadn't happened).
        {
            let mut wal = wal::Wal::open(&dir.path().join(WAL_SUBDIR), 1).unwrap();
            wal.append(&[test_span("aa", "01", 10)]).unwrap();
        }
        let store = open(dir.path()).await;
        assert_eq!(store.span_count().unwrap(), 1, "no double count");
        // and the stale segment was cleaned up on open
        let segs = wal::list_segment_seqnos(&dir.path().join(WAL_SUBDIR)).unwrap();
        assert!(!segs.contains(&1));
    }

    #[tokio::test]
    async fn orphan_parquet_is_swept_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path()).await;
            store.append(vec![test_span("aa", "01", 10)]).await.unwrap();
            store.seal_now().await.unwrap();
            store.compact_now().await.unwrap();
        }
        // Drop an unreferenced parquet file (as a crashed flush would leave behind).
        let orphan_dir = dir.path().join("blocks/1999/01/01/00");
        fs::create_dir_all(&orphan_dir).unwrap();
        let orphan = orphan_dir.join("orphan.parquet");
        fs::write(&orphan, b"not even valid parquet").unwrap();

        let _store = open(dir.path()).await; // open() sweeps orphans
        assert!(!orphan.exists(), "orphan parquet should be swept");
    }

    #[tokio::test]
    async fn background_compactor_flushes_sealed_segments() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                channel_capacity: 16,
                seal_threshold_spans: 100_000,
                compact_interval: Some(Duration::from_millis(50)),
                max_hot_spans: 0,
                blob_offload_bytes: 0,
            },
        )
        .unwrap();
        store.append(vec![test_span("aa", "01", 10)]).await.unwrap();
        store.seal_now().await.unwrap(); // makes the segment eligible

        // The background compactor (not compact_now) should pick it up within a few ticks.
        for _ in 0..50 {
            if store.index.watermark().unwrap() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            store.index.watermark().unwrap(),
            1,
            "compactor advanced watermark"
        );
        assert_eq!(store.hot.read().unwrap().len(), 0, "flushed group dropped");
        assert_eq!(store.span_count().unwrap(), 1, "served from cold");
    }
}
