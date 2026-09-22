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
pub mod format;
mod index;
pub mod merge;
mod scores;
mod wal;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::blob::BlobStore;
use crate::{NormalizedSpan, Score};
use index::Index;
pub use merge::{MergePolicy, MergeReport, MergeScope};
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
    /// The data-dir filesystem is below the configured free-space floor. Distinct from
    /// [`StoreError::Backpressure`]: a backlog drains on its own, so a client should retry
    /// shortly; this one clears only when someone frees space or retention runs.
    DiskFull {
        /// Bytes free when the guardrail last sampled.
        free_bytes: u64,
        /// The floor that was breached.
        min_free_bytes: u64,
    },
    /// A WAL write/fsync failed.
    Io(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Backpressure => write!(f, "ingest overloaded (backpressure)"),
            StoreError::Closed => write!(f, "store writer is closed"),
            StoreError::DiskFull {
                free_bytes,
                min_free_bytes,
            } => write!(
                f,
                "data-dir filesystem below the free-space floor ({} free, floor {})",
                crate::disk::human_bytes(*free_bytes),
                crate::disk::human_bytes(*min_free_bytes)
            ),
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
    /// Automatic retention: drop cold blocks whose spans ALL predate `now − window`.
    /// `None` disables it, which is the default — evald does not delete a user's data
    /// unless asked to.
    pub retention: Option<Duration>,
    /// How often the automatic retention sweep runs (ignored when `retention` is `None`).
    pub retention_interval: Duration,
    /// Hard stop: shed ingest while the data-dir filesystem has less than this many bytes
    /// free. `0` disables the floor.
    pub disk_min_free_bytes: u64,
    /// Warn (log + `evald_disk_low` metric) below this many free bytes. `0` disables.
    pub disk_warn_free_bytes: u64,
    /// How often free space is sampled. `None` disables the guardrail entirely (no probe,
    /// no floor) — ingest behaves exactly as it did before the guardrail existed.
    pub disk_check_interval: Option<Duration>,
    /// PII redaction policy applied before anything is written. `None` disables it, which is
    /// the default: rewriting a user's telemetry is irreversible (the raw value never
    /// reaches disk), so it is always an explicit choice.
    pub redactor: Option<crate::redact::Redactor>,
    /// Read-time policy for combining span scores into a trace-level score. Empty means the
    /// documented defaults (mean for numeric, all for boolean).
    pub rollup: crate::rollup::RollupConfig,
    /// Cold-to-cold compaction ([`merge`]): when an hour partition's flush blocks collapse
    /// into one, whether closed days collapse further, and how large a merged block may
    /// grow. The default merges; `hour_threshold: 0` leaves every flush block as written.
    pub merge: MergePolicy,
    /// The LLM usage series exposed on `/metrics` ([`crate::usage`]): on by default, with a
    /// cap on distinct model labels.
    pub usage: crate::usage::UsageConfig,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            seal_threshold_spans: 50_000,
            compact_interval: Some(Duration::from_secs(5)),
            // 300k un-compacted spans resident before shedding — **sized against a
            // container limit, not against a round number**, and against the whole process
            // rather than the tier alone. Measured peak across a full cycle at this bound
            // (fill, a bounded read, compaction, a merge, a SQL aggregate) is 738 MiB, which
            // with a serve process's own ~114 MiB is ~83% of the 1 GiB limit the shipped
            // k3s/Helm manifests set for a node — about 172 MiB spare for a query heavier
            // than the measurement ran. `HOT_TIER_DECISION.md` §4 has the table; 350k reaches
            // 92% of that limit and 500k exceeds it outright.
            //
            // Two things scale with this number: the spans (~1.9 KiB each at a 1 KiB
            // payload) and the read path's hot/cold dedup set (~0.07 KiB per resident span).
            // Compaction's own transient — one sealed segment cloned plus its Arrow batch,
            // ~155 MiB — scales with `seal_threshold_spans` instead, and is already counted.
            //
            // The original default of 1M was ~1.8 GiB in spans alone, i.e. past that limit
            // before anything else ran: the OOM killer arrived before the bound could shed,
            // which is precisely the failure the bound exists to prevent.
            //
            // Headroom: 6 sealed segments at the 50k seal threshold, and ~6x the
            // steady-state window of the measured fleet deployment (17.5k spans/s over a 3 s
            // compaction interval is ~52k spans resident). A store sustaining more than
            // ~60k spans/s at the default 5 s interval will exceed this in normal operation
            // and should raise both this and its memory limit — see `docs/CONFIG.md`.
            max_hot_spans: 300_000,
            // 256 KiB: well above a normal prompt/completion, below the RAG-context /
            // tool-output payloads that bloat the store and freeze the browser.
            blob_offload_bytes: 256 * 1024,
            // Off: deleting spans is the operator's decision, never a default.
            retention: None,
            retention_interval: Duration::from_secs(3600),
            // The guardrail is OFF in the library default and turned ON by `evald serve`
            // (which defaults `--disk-min-free` to 256MiB). The asymmetry is deliberate:
            // this default is also what one-shot commands (`query`, `cost`, `retention`)
            // and embedded users get, and spawning a filesystem-probing background task for
            // a command that opens the store, reads, and exits would be pure waste. It also
            // keeps the default free of any dependency on the host's free space, so a test
            // or a tool cannot start behaving differently because the build machine is full.
            // A long-running server is the only thing that can actually fill a disk, and it
            // opts in explicitly.
            disk_min_free_bytes: 0,
            disk_warn_free_bytes: 0,
            disk_check_interval: None,
            // Off: redaction is irreversible by design (the raw value never reaches disk),
            // so it is never something evald does to a user's data without being asked.
            redactor: None,
            rollup: crate::rollup::RollupConfig::default(),
            merge: MergePolicy::default(),
            usage: crate::usage::UsageConfig::default(),
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
    /// Cumulative spans durably ACK'd, counted at the fsync that commits them — so it
    /// tracks the durability boundary rather than requests received. Monotonic for the
    /// process lifetime: the `/metrics` counter an operator rates to get ingest/s.
    ingested: Arc<AtomicU64>,
    /// Background compaction passes completed / failed. Failures climbing while
    /// completions stall is the shape of a wedged compactor — which is what makes the hot
    /// tier grow until ingest starts shedding.
    compactions: Arc<AtomicU64>,
    compaction_failures: Arc<AtomicU64>,
    /// Free bytes as of the guardrail's last sample; [`u64::MAX`] means "not known"
    /// (guardrail disabled, or the probe failed), which is never treated as low.
    disk_free: Arc<AtomicU64>,
    /// Set while free space is under the floor. The ingest path reads this with one relaxed
    /// load, so the guardrail costs an append nothing when the disk is healthy.
    disk_blocked: Arc<AtomicBool>,
    /// Spans refused because of the floor — kept separate from `rejections` so an operator
    /// can tell "outrunning fsync" from "out of disk" without reading logs.
    disk_blocked_spans: Arc<AtomicU64>,
    /// Automatic-retention outcomes.
    retention_sweeps: Arc<AtomicU64>,
    retention_blocks_dropped: Arc<AtomicU64>,
    retention_bytes_reclaimed: Arc<AtomicU64>,
    /// The configured floor, for the error a blocked append returns.
    disk_min_free_bytes: u64,
    /// PII policy, applied by [`Store::prepare_for_storage`] before anything is written.
    redactor: Option<Arc<crate::redact::Redactor>>,
    /// Hits per redaction rule, parallel to `redactor.labels()`. One pre-allocated counter
    /// per rule rather than a map behind a lock: the rule set is fixed once the policy is
    /// built, so the ingest path only ever does a relaxed `fetch_add` on a known index.
    redaction_counts: Arc<Vec<AtomicU64>>,
    /// Read-time rollup policy (see [`crate::rollup`]).
    rollup: Arc<crate::rollup::RollupConfig>,
    /// Externalized store for offloaded oversized payloads + the offload size cap.
    blobs: Arc<BlobStore>,
    blob_offload_bytes: usize,
    /// The cold-merge policy (for [`Store::merge_now`]) and what merging has done so far.
    merge_policy: MergePolicy,
    merge_counters: MergeCounters,
    /// LLM usage series; `None` under `--no-usage-metrics`. Fed by the writer at the commit
    /// point (never by WAL replay) and by [`Store::put_scores`].
    usage: Option<Arc<crate::usage::UsageMetrics>>,
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
    /// Cumulative spans durably ACK'd since this process started (counted at the fsync
    /// that commits them).
    pub spans_ingested: u64,
    /// Background compaction passes completed since this process started.
    pub compactions: u64,
    /// Background compaction passes that failed since this process started.
    pub compaction_failures: u64,
    /// Free bytes on the data-dir filesystem at the guardrail's last sample; `None` when
    /// the guardrail is disabled or the probe could not read it.
    pub disk_free_bytes: Option<u64>,
    /// True while ingest is refused because free space is under the floor.
    pub disk_blocked: bool,
    /// Cumulative spans refused by the disk floor.
    pub disk_blocked_spans: u64,
    /// Automatic retention sweeps completed, and what they reclaimed.
    pub retention_sweeps: u64,
    pub retention_blocks_dropped: u64,
    pub retention_bytes_reclaimed: u64,
    /// Committed cold blocks right now — the file set a full scan opens, and the number
    /// cold-to-cold merging exists to bound.
    pub cold_blocks: u64,
    /// Cold-to-cold merges completed since this process started, the input blocks they
    /// consumed, and merge passes that failed.
    pub cold_merges: u64,
    pub cold_blocks_merged: u64,
    pub cold_merge_failures: u64,
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
        // The format marker comes first, before any directory exists: a data-dir written by
        // a newer format is refused here, untouched, and a pre-freeze data-dir is stamped
        // (docs/FORMAT.md). Creating `wal/` first would make an empty dir look like a legacy one.
        let format = format::ensure(data_dir)?;
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
        let ingested = Arc::new(AtomicU64::new(0));
        let compactions = Arc::new(AtomicU64::new(0));
        let compaction_failures = Arc::new(AtomicU64::new(0));
        // u64::MAX = "not known", which compares as plentiful — so a disabled or failing
        // probe can never be mistaken for a full disk.
        let disk_free = Arc::new(AtomicU64::new(u64::MAX));
        let disk_blocked = Arc::new(AtomicBool::new(false));
        let disk_blocked_spans = Arc::new(AtomicU64::new(0));
        let retention_sweeps = Arc::new(AtomicU64::new(0));
        let retention_blocks_dropped = Arc::new(AtomicU64::new(0));
        let retention_bytes_reclaimed = Arc::new(AtomicU64::new(0));
        let merge_counters = MergeCounters::default();
        let redactor = config.redactor.clone().map(Arc::new);
        let redaction_counts = Arc::new(
            redactor
                .as_ref()
                .map(|r| r.labels().iter().map(|_| AtomicU64::new(0)).collect())
                .unwrap_or_default(),
        );
        let usage = config
            .usage
            .enabled
            .then(|| Arc::new(crate::usage::UsageMetrics::new(&config.usage)));
        let (tx, rx) = mpsc::channel(channel_capacity);
        tokio::spawn(writer_loop(
            rx,
            wal,
            hot.clone(),
            active_seqno.clone(),
            config.seal_threshold_spans,
            config.max_hot_spans,
            rejections.clone(),
            ingested.clone(),
            usage.clone(),
        ));
        if let Some(interval) = config.compact_interval {
            spawn_compactor(
                data_dir.clone(),
                index.clone(),
                hot.clone(),
                active_seqno.clone(),
                compaction.clone(),
                interval,
                CompactionCounters {
                    passes: compactions.clone(),
                    failures: compaction_failures.clone(),
                },
                config.merge.clone(),
                merge_counters.clone(),
            );
        }

        if let Some(interval) = config.disk_check_interval {
            spawn_disk_monitor(
                data_dir.clone(),
                DiskGuard {
                    free: disk_free.clone(),
                    blocked: disk_blocked.clone(),
                    min_free_bytes: config.disk_min_free_bytes,
                    warn_free_bytes: config.disk_warn_free_bytes,
                },
                interval,
            );
        }
        if let Some(window) = config.retention {
            spawn_retention(
                data_dir.clone(),
                index.clone(),
                compaction.clone(),
                window,
                config.retention_interval,
                RetentionCounters {
                    sweeps: retention_sweeps.clone(),
                    blocks_dropped: retention_blocks_dropped.clone(),
                    bytes_reclaimed: retention_bytes_reclaimed.clone(),
                },
            );
        }

        tracing::info!(
            spans_recovered = recovered,
            watermark,
            format = format.format,
            "store opened"
        );
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
            ingested,
            compactions,
            compaction_failures,
            disk_free,
            disk_blocked,
            disk_blocked_spans,
            retention_sweeps,
            retention_blocks_dropped,
            retention_bytes_reclaimed,
            disk_min_free_bytes: config.disk_min_free_bytes,
            redactor,
            redaction_counts,
            rollup: Arc::new(config.rollup.clone()),
            blobs,
            blob_offload_bytes: config.blob_offload_bytes,
            merge_policy: config.merge,
            merge_counters,
            usage,
        })
    }

    /// Offload any oversized `input_value` / `output_value` on these spans to the blob store,
    /// in place, replacing the field with an `evald-blob:<key>` reference. Called at ingest
    /// **before** [`Store::append`], so the WAL and Parquet blocks only ever carry the compact
    /// reference. Returns the number of fields offloaded. A no-op when offloading is disabled.
    pub fn offload_payloads(&self, spans: &mut [NormalizedSpan]) -> usize {
        crate::blob::offload_large_payloads(spans, &self.blobs, self.blob_offload_bytes)
    }

    /// The one transformation every span passes through on its way to durable storage:
    /// **redact, then offload**. Both ingest front doors (HTTP and gRPC) call this rather
    /// than the two steps separately, so neither can grow a path that skips one.
    ///
    /// The order is the whole point. Redaction must precede the offload, or an oversized
    /// prompt would have its raw text written to a blob file before anything scanned it —
    /// and it must precede the append, because the WAL is the ACK boundary and a value that
    /// reaches it is durable by definition. Run in this order, a detected value never
    /// touches disk in any tier: not the WAL, not a blob, not a Parquet block.
    pub fn prepare_for_storage(&self, spans: &mut [NormalizedSpan]) -> usize {
        if let Some(redactor) = &self.redactor {
            for span in spans.iter_mut() {
                for (rule, hits) in redactor.scrub_span(span) {
                    if let Some(counter) = self.redaction_counts.get(rule) {
                        counter.fetch_add(hits, Ordering::Relaxed);
                    }
                }
            }
        }
        self.offload_payloads(spans)
    }

    /// Per-rule redaction hit counts as `(label, count)`, for `/metrics` and `/v1/stats`.
    /// Empty when no policy is configured.
    pub fn redaction_counts(&self) -> Vec<(String, u64)> {
        let Some(redactor) = &self.redactor else {
            return Vec::new();
        };
        redactor
            .labels()
            .iter()
            .zip(self.redaction_counts.iter())
            .map(|(label, n)| (label.clone(), n.load(Ordering::Relaxed)))
            .collect()
    }

    /// Fetch an offloaded payload by its blob key (or full `evald-blob:<key>` reference).
    /// `Ok(None)` for an unknown / malformed key.
    pub fn get_blob(&self, key_or_ref: &str) -> io::Result<Option<Vec<u8>>> {
        self.blobs.get(key_or_ref)
    }

    /// A snapshot of ingest load (backlog / channel depth / cumulative sheds) for the stats
    /// endpoint. Cheap: one hot-tier read lock, one redb read transaction for the block
    /// count (an O(1) header read, not a scan), and atomic loads.
    pub fn ingest_stats(&self) -> IngestStats {
        let hot_spans = self.hot.read().expect("hot tier lock").len();
        let shedding = self.max_hot_spans != 0 && hot_spans >= self.max_hot_spans;
        // A gauge: a failed read is worth less than a failed scrape, so it reports 0.
        let cold_blocks = self.index.block_count().unwrap_or(0);
        IngestStats {
            hot_spans,
            max_hot_spans: self.max_hot_spans,
            channel_capacity: self.channel_capacity,
            rejections: self.rejections.load(Ordering::Relaxed),
            shedding,
            spans_ingested: self.ingested.load(Ordering::Relaxed),
            compactions: self.compactions.load(Ordering::Relaxed),
            compaction_failures: self.compaction_failures.load(Ordering::Relaxed),
            disk_free_bytes: match self.disk_free.load(Ordering::Relaxed) {
                u64::MAX => None,
                n => Some(n),
            },
            disk_blocked: self.disk_blocked.load(Ordering::Relaxed),
            disk_blocked_spans: self.disk_blocked_spans.load(Ordering::Relaxed),
            retention_sweeps: self.retention_sweeps.load(Ordering::Relaxed),
            retention_blocks_dropped: self.retention_blocks_dropped.load(Ordering::Relaxed),
            retention_bytes_reclaimed: self.retention_bytes_reclaimed.load(Ordering::Relaxed),
            cold_blocks,
            cold_merges: self.merge_counters.merges.load(Ordering::Relaxed),
            cold_blocks_merged: self.merge_counters.blocks_merged.load(Ordering::Relaxed),
            cold_merge_failures: self.merge_counters.failures.load(Ordering::Relaxed),
        }
    }

    /// Whether the writer task is still running — the one condition under which this node
    /// can no longer make an accepted span durable.
    ///
    /// The writer owns the only [`Cmd`] receiver, so if it panics or exits the receiver
    /// drops and the sender closes. That is a terminal, process-level fault: every
    /// subsequent `append` fails, and no retry or backoff recovers it. It is deliberately
    /// the ONLY input to readiness — shedding is not a fault (it is the documented
    /// backpressure contract, answered with `429 + Retry-After`), so a node under load
    /// stays ready and keeps telling clients to slow down, rather than being pulled from
    /// rotation exactly when the fleet needs it to apply backpressure.
    pub fn writer_is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    /// The disk floor, as a standalone check: `Err(DiskFull)` while free space is under
    /// `--disk-min-free`, counting `spans` as refused.
    ///
    /// Checked BEFORE the WAL append rather than after a failed write: the point is to
    /// refuse cleanly while there is still room to finish whatever is in flight, rather
    /// than discover `ENOSPC` halfway through a segment. One relaxed load on the healthy
    /// path.
    ///
    /// Public because [`Self::append`] is not the ingest path's first write to disk.
    /// [`Self::prepare_for_storage`] offloads oversized payloads to the blob store, and
    /// that runs first — so an ingest handler that only relies on `append`'s guard writes
    /// blob files onto an already-full volume and then answers 503, leaving them behind
    /// with nothing referencing them. A caller that offloads must call this before it, and
    /// `append` still calls it too: the guardrail samples free space on a timer, so the
    /// floor can be crossed between the two.
    pub fn reject_if_disk_full(&self, spans: usize) -> Result<(), StoreError> {
        // An empty batch writes nothing, so there is nothing for the floor to protect. It
        // must not be refused: `append` documents an empty batch as a no-op, and a `503`
        // for a request that was never going to touch the disk sends an OTLP exporter into
        // a retry loop over nothing — invisibly, since `evald_spans_disk_blocked_total`
        // would move by zero.
        if spans == 0 {
            return Ok(());
        }
        if self.disk_blocked.load(Ordering::Relaxed) {
            self.disk_blocked_spans
                .fetch_add(spans as u64, Ordering::Relaxed);
            return Err(StoreError::DiskFull {
                free_bytes: self.disk_free.load(Ordering::Relaxed),
                min_free_bytes: self.disk_min_free_bytes,
            });
        }
        Ok(())
    }

    /// Durably append a batch of spans (returns once fsynced to the WAL), or a
    /// [`StoreError`] if shed or failed. An empty batch is a no-op.
    pub async fn append(&self, spans: Vec<NormalizedSpan>) -> Result<(), StoreError> {
        // Answered here rather than by the writer task, so "no-op" means no channel slot
        // and no fsync wait — and so an empty batch cannot be turned into a `Backpressure`
        // error by a full channel it was never going to add to.
        if spans.is_empty() {
            return Ok(());
        }
        self.reject_if_disk_full(spans.len())?;
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

    /// Run one cold-to-cold merge pass now ([`merge::merge_cold`]) with the configured
    /// policy, synchronously. `MergeScope::Full` is what `evald compact` runs: every hour
    /// partition with more than one block, plus closed days, regardless of the tick
    /// threshold. Counted in `/v1/stats` like a background merge unless `dry_run`.
    pub async fn merge_now(&self, scope: MergeScope, dry_run: bool) -> io::Result<MergeReport> {
        let data_dir = self.data_dir.clone();
        let index = self.index.clone();
        let compaction = self.compaction.clone();
        let policy = self.merge_policy.clone();
        let report = tokio::task::spawn_blocking(move || {
            merge::merge_cold(&data_dir, &index, &compaction, &policy, scope, dry_run)
        })
        .await
        .map_err(io::Error::other)??;
        if !dry_run {
            self.merge_counters.record(&report);
        }
        Ok(report)
    }

    /// Unlink un-indexed Parquet under `blocks/` older than `older_than`
    /// ([`merge::sweep_unindexed`]) — merge inputs past their grace and crashed outputs.
    /// The compactor runs this every tick with the policy's grace; this is the explicit
    /// form for `evald compact` and tests. Returns the number of files removed.
    pub async fn sweep_unindexed(&self, older_than: Duration) -> io::Result<usize> {
        let data_dir = self.data_dir.clone();
        let index = self.index.clone();
        let compaction = self.compaction.clone();
        tokio::task::spawn_blocking(move || {
            merge::sweep_unindexed(&data_dir, &index, &compaction, older_than)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// The number of committed cold blocks (the index's count, never a directory scan).
    pub fn cold_block_count(&self) -> io::Result<u64> {
        self.index.block_count()
    }

    /// The configured cold-merge policy.
    pub fn merge_policy(&self) -> &MergePolicy {
        &self.merge_policy
    }

    /// Most-recent-first spans (hot ∪ cold, deduped), optionally filtered by `trace_id`.
    pub fn query(&self, trace_id: Option<&str>, limit: usize) -> io::Result<Vec<NormalizedSpan>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Every resident span is keyed so a cold block cannot re-emit one the hot tier
        // already has. That set is unavoidable — deciding it per cold span by scanning the
        // hot tier would be quadratic — so the cost that matters is bytes per entry, and
        // [`SpanKey`] keeps a canonical OTLP identity in 32 of them with no heap at all.
        let mut hot_seen: HashSet<SpanKey> = HashSet::new();

        // Hot first — un-flushed, freshest.
        //
        // Selected into a bounded top-`limit` heap of BORROWED spans, so the only spans
        // cloned are the ones that survive. Collecting every match and truncating afterwards
        // is the obvious version and it copies the whole hot tier to answer
        // `GET /v1/spans?limit=100` — at the shed threshold that is hundreds of megabytes
        // allocated and dropped per request, which is memory an operator then has to
        // reserve (`HOT_TIER_DECISION.md` §4).
        //
        // Dropping a hot span here cannot lose a result: one outside hot's own top-`limit`
        // is beaten by `limit` hot spans, so it cannot be in the top-`limit` of hot ∪ cold
        // either. `seen` still records EVERY hot key, so a hot span that lost still
        // suppresses its cold copy — hot wins on identity regardless of rank.
        let mut rows = {
            let hot = self.hot.read().expect("hot tier lock");
            // Sized up front for an unfiltered read, where every resident span is a
            // candidate. Growing a hash table by doubling holds the old and the new one at
            // once, and that transient is real memory at this size. A trace-filtered read
            // matches a handful, so it is left to grow.
            if trace_id.is_none() {
                hot_seen.reserve(hot.len());
            }
            match Rows::new(limit) {
                Rows::Top(_) => {
                    // Borrowed until the selection is settled: only survivors are cloned.
                    let mut refs: TopN<&NormalizedSpan> = TopN::new(limit);
                    for s in hot.all_spans() {
                        if trace_matches(trace_id, s) && hot_seen.insert(span_key(s)) {
                            refs.push(s.start_unix_nano, s);
                        }
                    }
                    Rows::Top(refs.cloned())
                }
                Rows::All(mut all) => {
                    for s in hot.all_spans() {
                        if trace_matches(trace_id, s) && hot_seen.insert(span_key(s)) {
                            all.push(s.clone());
                        }
                    }
                    Rows::All(all)
                }
            }
        };
        // Cold — only blocks the index says are committed (never a raw dir scan). A merge
        // can swap blocks out of the index between this listing and the reads below; its
        // inputs stay on disk for the merge grace, so the read normally still succeeds. If
        // one is gone anyway (a grace shorter than the query, or `evald compact` between
        // two runs), re-list once and read the current set rather than fail the query on
        // a file the index no longer names.
        let mut attempts = 0;
        loop {
            attempts += 1;
            let block_paths = match trace_id {
                Some(t) => self.index.block_paths_for_trace(t)?,
                None => self.index.all_block_paths()?,
            };
            // Cold candidates are selected on their own and merged in once the attempt
            // succeeds, so a partly-read attempt is simply discarded — and the hot result is
            // never copied to make a retry possible.
            //
            // `cold_seen` is its own set rather than a clone of the hot one. A clone would be
            // a second copy of the largest structure here, allocated on every read, to hold
            // keys the hot set already has; keeping them separate and testing both costs one
            // extra lookup per cold span and no memory.
            let mut cold = Rows::new(limit);
            let mut cold_seen: HashSet<SpanKey> = HashSet::new();
            let mut vanished = false;
            for rel in block_paths {
                let spans = match cold::read_block(&self.data_dir.join(&rel)) {
                    Ok(spans) => spans,
                    Err(e) if e.kind() == io::ErrorKind::NotFound && attempts == 1 => {
                        tracing::debug!(%rel, "cold block vanished mid-query; re-listing");
                        vanished = true;
                        break;
                    }
                    Err(e) => return Err(e),
                };
                for s in spans {
                    if !trace_matches(trace_id, &s) {
                        continue;
                    }
                    let key = span_key(&s);
                    if !hot_seen.contains(&key) && cold_seen.insert(key) {
                        cold.push(s.start_unix_nano, s);
                    }
                }
            }
            if vanished {
                continue;
            }
            // Merged in result order, so cold spans keep their relative order behind the hot
            // ones — the same tie-break the single-pass stable sort gave.
            for s in cold.into_vec() {
                rows.push(s.start_unix_nano, s);
            }
            break;
        }

        Ok(rows.into_vec())
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
        self.scores.put_batch(scores)?;
        if let Some(usage) = &self.usage {
            usage.record_scores(scores);
        }
        Ok(())
    }

    /// The LLM usage series, or `None` when disabled (`--no-usage-metrics`).
    pub fn usage(&self) -> Option<&crate::usage::UsageMetrics> {
        self.usage.as_deref()
    }

    /// A single score by id.
    pub fn get_score(&self, id: &str) -> io::Result<Option<Score>> {
        self.scores.get(id)
    }

    /// All scores for a target (e.g. a span or trace), newest-first.
    pub fn scores_for_target(&self, target: &crate::ScoreTarget) -> io::Result<Vec<Score>> {
        self.scores.by_target_key(&target.key())
    }

    /// Every score attached to a trace **or to any of its spans**.
    ///
    /// `scores_for_target(Trace(id))` deliberately answers a narrower question — scores
    /// stated about the trace itself — which is why a trace whose every span is scored looks
    /// unscored through it. This is the gathering step [`Store::trace_rollup`] needs.
    pub fn trace_scores(&self, trace_id: &str) -> io::Result<Vec<Score>> {
        let mut out = self.scores_for_target(&crate::ScoreTarget::Trace(trace_id.to_string()))?;
        for span in self.trace(trace_id)? {
            out.extend(self.scores_for_target(&crate::ScoreTarget::Span(span.span_id))?);
        }
        Ok(out)
    }

    /// Trace-level scores: measured where the trace itself was scored, derived from its
    /// spans otherwise, absent where nothing carries the name. See [`crate::rollup`].
    pub fn trace_rollup(&self, trace_id: &str) -> io::Result<Vec<crate::rollup::RolledScore>> {
        let spans = self.trace(trace_id)?;
        let scores = self.trace_scores(trace_id)?;
        Ok(crate::rollup::rollup_trace(
            trace_id,
            &spans,
            &scores,
            &self.rollup,
        ))
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

    /// Total bytes currently held in the write-ahead log.
    ///
    /// Cheap enough for a metrics scrape by construction: the WAL holds only segments not
    /// yet compacted to Parquet, so the directory is bounded by the seal threshold and the
    /// compactor's lag — a handful of files, not the whole store. (The cold `blocks/` tree
    /// and the score table are deliberately NOT summed here: both grow without bound, and
    /// an O(n) walk on every scrape is how a monitoring endpoint becomes the outage. Their
    /// inventory is available through `/v1/sql`, where the caller chooses to pay for it.)
    ///
    /// A segment that vanishes mid-walk (the compactor truncating underneath us) is
    /// skipped rather than erroring: this is a gauge, and a torn read is worth less than a
    /// failed scrape.
    pub fn wal_bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.data_dir.join(WAL_SUBDIR)) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

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
        reclaim_before_in(
            &self.data_dir,
            &self.index,
            &self.compaction,
            cutoff_unix_nano,
            dry_run,
        )
    }
}

/// The sweep itself, as a free function over the pieces it needs rather than a `Store`
/// method body.
///
/// The background sweep ([`spawn_retention`]) must NOT hold a `Store`: the compactor's
/// precedent is to capture only the Arcs it uses, because a long-lived task holding a
/// `Store` would also hold its `mpsc::Sender`, keeping the writer channel open for the
/// life of the process — which would make [`Store::writer_is_alive`], and therefore
/// `/readyz`, unable to ever report a dead writer.
fn reclaim_before_in(
    data_dir: &Path,
    index: &Index,
    compaction: &std::sync::Mutex<()>,
    cutoff_unix_nano: u64,
    dry_run: bool,
) -> io::Result<ReclaimReport> {
    // Recover from poisoning rather than propagating it, matching `flush_pending`. The lock
    // guards nothing but mutual exclusion between the compactor and this sweep — there is no
    // invariant a panicking holder could leave half-applied, because both publish by atomic
    // rename. `expect` here would be strictly worse than the panic it reports: the retention
    // task runs this on a timer inside `spawn_blocking`, so ONE poisoning panic anywhere in
    // compaction would make every later tick panic on the lock itself, and the process would
    // log "retention task panicked (will retry)" forever without reclaiming another byte.
    let _guard = compaction.lock().unwrap_or_else(|p| p.into_inner());

    let mut drop_paths: Vec<String> = Vec::new();
    let mut bytes_reclaimed: u64 = 0;
    let mut blocks_kept = 0usize;
    let mut oldest_kept: Option<u64> = None;
    for (rel, max_start) in index.blocks_with_max_start()? {
        if max_start < cutoff_unix_nano {
            // The block's newest span predates the cutoff → every span in it is expired.
            if let Ok(meta) = fs::metadata(data_dir.join(&rel)) {
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
        index.drop_blocks(&drop_paths)?;
        for rel in &drop_paths {
            match fs::remove_file(data_dir.join(rel)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        remove_empty_partition_dirs(&data_dir.join(BLOCKS_SUBDIR));
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

fn span_key(s: &NormalizedSpan) -> SpanKey {
    match (decode_hex::<16>(&s.trace_id), decode_hex::<8>(&s.span_id)) {
        (Some(trace), Some(span)) => {
            let mut k = [0u8; 24];
            k[..16].copy_from_slice(&trace);
            k[16..].copy_from_slice(&span);
            SpanKey::Otlp(k)
        }
        _ => SpanKey::Other(Box::new((s.trace_id.clone(), s.span_id.clone()))),
    }
}

/// A span's identity for de-duplication.
///
/// The read path keys every resident span, so this type's size is multiplied by the hot
/// tier's bound. The obvious `(String, String)` costs 48 bytes in the table plus two heap
/// allocations per span; the ids evald actually stores are a 16-byte trace id and an 8-byte
/// span id written as lowercase hex, so decoding them back to bytes is **exact** and fits in
/// 32 bytes with no heap at all.
///
/// Exact matters more than small here. Hashing the pair into a fixed-width key would be
/// simpler and smaller still, but a collision would silently drop a span from a query
/// result — the one failure this store refuses to have, and one no test could reliably
/// catch. Decoding cannot collide: the bytes are a bijection with the canonical hex, and a
/// non-canonical id takes the other variant, which is never equal to it.
#[derive(Debug, PartialEq, Eq, Hash)]
enum SpanKey {
    /// The canonical case: trace id bytes followed by span id bytes.
    Otlp([u8; 24]),
    /// An id that is not canonical hex — one an embedded caller invented, or uppercase hex.
    /// Held as a pair, not a joined string, so no separator can make two different pairs
    /// compare equal. Boxed to keep the common variant's size down.
    Other(Box<(String, String)>),
}

/// `N` bytes from exactly `2N` lowercase hex digits, or `None` for anything else.
fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        // Lowercase only: the normalizer writes lowercase, and accepting both would make
        // two spellings of one id two different keys anyway — this way the odd one out
        // simply takes the `Other` variant instead of silently half-matching.
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = (nibble(bytes[i * 2])? << 4) | nibble(bytes[i * 2 + 1])?;
    }
    Some(out)
}

/// How a read accumulates its result. The two arms exist because the trade differs at the
/// extremes, and the read path has both: `GET /v1/spans?limit=100` wants a bounded
/// selection, while [`Store::trace`] and [`Store::span_count`] ask for everything and a
/// bound would be pure overhead.
enum Rows {
    /// Bounded: keep at most `limit`, dropping candidates as they are seen.
    Top(TopN<NormalizedSpan>),
    /// Unbounded (`usize::MAX`): collect and stable-sort. A heap here would pay `O(log n)`
    /// moves of a large struct per span to bound something the caller has said is not
    /// bounded — measured at 5.9 s against 3.5 s for a million-span count.
    All(Vec<NormalizedSpan>),
}

impl Rows {
    fn new(limit: usize) -> Self {
        if limit == usize::MAX {
            Rows::All(Vec::new())
        } else {
            Rows::Top(TopN::new(limit))
        }
    }

    fn push(&mut self, start: u64, span: NormalizedSpan) {
        match self {
            Rows::Top(top) => top.push(start, span),
            Rows::All(all) => all.push(span),
        }
    }

    /// Newest first, ties in insertion order.
    fn into_vec(self) -> Vec<NormalizedSpan> {
        match self {
            Rows::Top(top) => top.into_vec(),
            Rows::All(mut all) => {
                // Stable, so equal start times keep insertion order — hot before cold.
                all.sort_by_key(|s| std::cmp::Reverse(s.start_unix_nano));
                all
            }
        }
    }
}

/// The newest `limit` items, selected without holding more than `limit` of them.
///
/// The read path answers "most recent N spans", and the candidate set is the whole hot tier
/// plus every committed block. Collecting all of it and sorting is `O(n)` memory for an
/// `O(limit)` answer; this keeps a bounded heap whose top is the item that loses next, so a
/// candidate that cannot make the cut is dropped as it is seen.
///
/// Order is the documented one — newest first, ties broken by insertion — and it is exactly
/// what the previous stable `sort_by_key(Reverse(start))` produced, so a caller cannot tell
/// the two apart by the rows it gets back.
struct TopN<T> {
    heap: std::collections::BinaryHeap<Loser<T>>,
    limit: usize,
    seq: usize,
}

impl<T> TopN<T> {
    fn new(limit: usize) -> Self {
        TopN {
            // `limit` is caller-supplied and may be `usize::MAX` (`trace`, `span_count`), so
            // it is a bound to compare against, never a capacity to allocate up front.
            heap: std::collections::BinaryHeap::new(),
            limit,
            seq: 0,
        }
    }

    fn push(&mut self, start: u64, item: T) {
        self.heap.push(Loser {
            start,
            seq: self.seq,
            item,
        });
        self.seq += 1;
        if self.heap.len() > self.limit {
            self.heap.pop(); // the heap's max is the loser, by `Loser`'s ordering
        }
    }

    /// Newest first, ties in insertion order.
    fn into_vec(self) -> Vec<T> {
        // `into_sorted_vec` is ascending by `Ord`, and `Loser` orders "loses sooner" as
        // greater — so ascending is already best-first.
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|l| l.item)
            .collect()
    }
}

impl TopN<&NormalizedSpan> {
    /// Clone the survivors out from under the hot-tier lock, keeping their insertion order
    /// so a later cold span ties against them the same way it would have.
    fn cloned(self) -> TopN<NormalizedSpan> {
        TopN {
            heap: self
                .heap
                .into_iter()
                .map(|l| Loser {
                    start: l.start,
                    seq: l.seq,
                    item: l.item.clone(),
                })
                .collect(),
            limit: self.limit,
            seq: self.seq,
        }
    }
}

/// A candidate ordered so that **greater means "loses first"**: oldest start, and among
/// equal starts the one inserted later. That makes `BinaryHeap`'s max exactly the item to
/// evict when the heap is over capacity, and its ascending sort exactly the output order.
struct Loser<T> {
    start: u64,
    seq: usize,
    item: T,
}

impl<T> PartialEq for Loser<T> {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.seq == other.seq
    }
}
impl<T> Eq for Loser<T> {}
impl<T> Ord for Loser<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse on start (older loses first), then later insertion loses first.
        other
            .start
            .cmp(&self.start)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}
impl<T> PartialOrd for Loser<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
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
    ingested: Arc<AtomicU64>,
    usage: Option<Arc<crate::usage::UsageMetrics>>,
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
                        // Counted HERE, after the fsync that commits the group and before
                        // the ACK, so the counter means exactly what the durability claim
                        // does: spans that survive a kill -9. Spans encoded in phase 1 but
                        // lost to an fsync failure are never counted.
                        ingested.fetch_add(spans.len() as u64, Ordering::Relaxed);
                        // The usage series are recorded at the same commit point, one lock
                        // per batch. Recovery never reaches here (it loads the WAL in
                        // `Store::open`), so a restart does not re-count recovered spans.
                        if let Some(usage) = &usage {
                            usage.record(&spans);
                        }
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

/// What [`spawn_disk_monitor`] publishes into, plus the thresholds it compares against.
struct DiskGuard {
    free: Arc<AtomicU64>,
    blocked: Arc<AtomicBool>,
    min_free_bytes: u64,
    warn_free_bytes: u64,
}

/// Sample free space on the data-dir filesystem on a timer and publish the verdict.
///
/// On a timer rather than per-append on purpose: `statvfs` is a syscall, and paying for one
/// per ingest request to answer a question whose answer changes over minutes would tax the
/// hot path for nothing. The ingest side reads one relaxed bool.
///
/// Three transitions are logged once each, not per sample — a guardrail that logs every 10s
/// while a disk is full buries the line that says it filled.
fn spawn_disk_monitor(data_dir: Arc<PathBuf>, guard: DiskGuard, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut was_blocked = false;
        let mut was_warned = false;
        let mut probe_failed = false;
        loop {
            tick.tick().await;
            let dir = data_dir.clone();
            let free = tokio::task::spawn_blocking(move || crate::disk::free_bytes(&dir)).await;
            let free = match free {
                Ok(Some(free)) => {
                    probe_failed = false;
                    free
                }
                // FAIL OPEN. An unreadable probe means we do not know, and refusing ingest
                // on "do not know" would invent an outage the disk never had.
                Ok(None) | Err(_) => {
                    if !probe_failed {
                        probe_failed = true;
                        tracing::warn!(
                            data_dir = %data_dir.display(),
                            "disk guardrail: free space could not be read — guardrail inactive"
                        );
                    }
                    guard.free.store(u64::MAX, Ordering::Relaxed);
                    guard.blocked.store(false, Ordering::Relaxed);
                    continue;
                }
            };
            guard.free.store(free, Ordering::Relaxed);

            let blocked = guard.min_free_bytes != 0 && free < guard.min_free_bytes;
            guard.blocked.store(blocked, Ordering::Relaxed);

            if blocked && !was_blocked {
                tracing::error!(
                    free = %crate::disk::human_bytes(free),
                    floor = %crate::disk::human_bytes(guard.min_free_bytes),
                    "disk guardrail: BELOW FLOOR — shedding ingest (503) until space is freed"
                );
            } else if !blocked && was_blocked {
                tracing::info!(
                    free = %crate::disk::human_bytes(free),
                    "disk guardrail: recovered — accepting ingest again"
                );
                was_warned = false;
            }
            was_blocked = blocked;

            let warn = guard.warn_free_bytes != 0 && free < guard.warn_free_bytes;
            if warn && !blocked && !was_warned {
                was_warned = true;
                tracing::warn!(
                    free = %crate::disk::human_bytes(free),
                    warn_at = %crate::disk::human_bytes(guard.warn_free_bytes),
                    floor = %crate::disk::human_bytes(guard.min_free_bytes),
                    "disk guardrail: free space is low — ingest will shed at the floor"
                );
            } else if !warn {
                was_warned = false;
            }
        }
    });
}

/// What [`spawn_retention`] records.
struct RetentionCounters {
    sweeps: Arc<AtomicU64>,
    blocks_dropped: Arc<AtomicU64>,
    bytes_reclaimed: Arc<AtomicU64>,
}

/// Run the retention sweep on a timer, dropping cold blocks entirely older than `window`.
///
/// Shares `reclaim_before`'s compaction mutex, so a sweep and a compaction pass never touch
/// the same block; a sweep that cannot take the lock simply waits for the next tick rather
/// than queueing up behind one.
fn spawn_retention(
    data_dir: Arc<PathBuf>,
    index: Arc<Index>,
    compaction: Arc<std::sync::Mutex<()>>,
    window: Duration,
    interval: Duration,
    counters: RetentionCounters,
) {
    tokio::spawn(async move {
        // First tick one interval out: a sweep racing the recovery that just repopulated
        // the index at startup would be reading a moving target for no benefit.
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(cutoff) = retention_cutoff(window) else {
                tracing::error!("retention: system clock is before the unix epoch — skipping");
                continue;
            };
            let (dir, idx, lock) = (data_dir.clone(), index.clone(), compaction.clone());
            let result = tokio::task::spawn_blocking(move || {
                reclaim_before_in(&dir, &idx, &lock, cutoff, false)
            })
            .await;
            match result {
                Ok(Ok(report)) => {
                    counters.sweeps.fetch_add(1, Ordering::Relaxed);
                    counters
                        .blocks_dropped
                        .fetch_add(report.blocks_dropped as u64, Ordering::Relaxed);
                    counters
                        .bytes_reclaimed
                        .fetch_add(report.bytes_reclaimed, Ordering::Relaxed);
                    // Only speak when something happened — a nightly sweep that drops
                    // nothing should not produce a log line every interval forever.
                    if report.blocks_dropped > 0 {
                        tracing::info!(
                            blocks = report.blocks_dropped,
                            reclaimed = %crate::disk::human_bytes(report.bytes_reclaimed),
                            kept = report.blocks_kept,
                            "retention: swept cold blocks past the window"
                        );
                    }
                }
                Ok(Err(e)) => tracing::error!(%e, "retention sweep failed (will retry)"),
                Err(e) => tracing::error!(%e, "retention task panicked (will retry)"),
            }
        }
    });
}

/// The retention cutoff for a window: `now − window` as unix nanos, or `None` if the system
/// clock predates the epoch. Shared by the CLI and the background sweep so the two cannot
/// drift into computing different cutoffs from the same window.
pub fn retention_cutoff(window: Duration) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(now.saturating_sub(window.as_nanos()).min(u64::MAX as u128) as u64)
}

/// The compactor's two counters. Grouped because they are always constructed, passed and
/// interpreted together — a pass either completes or fails, and the ratio is the signal.
/// Keeping them one value also keeps `spawn_compactor` inside clippy's argument budget
/// without reaching for another `#[allow(too_many_arguments)]`.
#[derive(Clone)]
struct CompactionCounters {
    passes: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
}

/// Cold-to-cold merge outcomes, shared by the compactor's tick and [`Store::merge_now`].
#[derive(Clone, Default)]
struct MergeCounters {
    /// Merged blocks written.
    merges: Arc<AtomicU64>,
    /// Input blocks consumed by those merges.
    blocks_merged: Arc<AtomicU64>,
    /// Merge passes that failed (the flush before them still counts as a compaction pass).
    failures: Arc<AtomicU64>,
}

impl MergeCounters {
    fn record(&self, report: &MergeReport) {
        self.merges
            .fetch_add(report.merges as u64, Ordering::Relaxed);
        self.blocks_merged
            .fetch_add(report.blocks_in as u64, Ordering::Relaxed);
    }
}

/// Spawn the background compactor: every `interval`, flush sealed segments to cold, then
/// merge cold blocks that are due ([`merge::merge_cold`]) and reclaim merge inputs past
/// their grace ([`merge::sweep_unindexed`]).
#[allow(clippy::too_many_arguments)]
fn spawn_compactor(
    data_dir: Arc<PathBuf>,
    index: Arc<Index>,
    hot: Arc<RwLock<HotTier>>,
    active_seqno: Arc<AtomicU64>,
    compaction: Arc<std::sync::Mutex<()>>,
    interval: Duration,
    counters: CompactionCounters,
    merge_policy: MergePolicy,
    merge_counters: MergeCounters,
) {
    tokio::spawn(async move {
        // Start one interval out, not immediately, so opening the store doesn't kick a
        // compaction pass that races an explicit `compact_now`.
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let result = {
                let (data_dir, index, hot, active_seqno, compaction) = (
                    data_dir.clone(),
                    index.clone(),
                    hot.clone(),
                    active_seqno.clone(),
                    compaction.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    flush_pending(&data_dir, &index, &hot, &active_seqno, &compaction)
                })
                .await
            };
            match result {
                Ok(Ok(())) => {
                    counters.passes.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Err(e)) => {
                    // A join error (the blocking task panicked) counts as a failed pass
                    // too — from the operator's side both mean "this pass did not drain
                    // the hot tier", which is the signal the metric exists to carry.
                    counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(%e, "compaction pass failed (will retry)");
                }
                Err(e) => {
                    counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(%e, "compaction task panicked (will retry)");
                }
            }

            // Cold-to-cold: merge what is due, then reclaim inputs past their grace. Kept
            // after the flush and counted separately so a merge failure never masks a
            // healthy flush (or the reverse) in the metrics — and never blocks it: the
            // flush is what drains the hot tier, and it already ran.
            let merged = {
                let (data_dir, index, compaction, policy) = (
                    data_dir.clone(),
                    index.clone(),
                    compaction.clone(),
                    merge_policy.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    let report = merge::merge_cold(
                        &data_dir,
                        &index,
                        &compaction,
                        &policy,
                        MergeScope::Tick,
                        false,
                    )?;
                    let swept = merge::sweep_unindexed(
                        &data_dir,
                        &index,
                        &compaction,
                        policy.unlink_grace,
                    )?;
                    Ok::<_, io::Error>((report, swept))
                })
                .await
            };
            match merged {
                Ok(Ok((report, swept))) => {
                    merge_counters.record(&report);
                    if !report.is_noop() || swept > 0 {
                        tracing::info!(
                            merges = report.merges,
                            blocks_in = report.blocks_in,
                            spans = report.spans,
                            bytes_in = report.bytes_in,
                            bytes_out = report.bytes_out,
                            swept,
                            "merged cold blocks"
                        );
                    }
                }
                Ok(Err(e)) => {
                    merge_counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(%e, "cold merge failed (will retry)");
                }
                Err(e) => {
                    merge_counters.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(%e, "cold merge task panicked (will retry)");
                }
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
                // The table in force now. The spans were priced up to a seal and a compaction
                // interval ago, so a reload in between can make this one version newer than
                // what priced some of them (docs/FORMAT.md); the per-span truth is a re-price.
                &cold::Provenance::flush(seqno)
                    .with_price_tables(vec![crate::price::current().version.clone()]),
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
    // Counted and reported ONCE, not logged per file. On a store that merges, every merge
    // input is unreferenced from its commit until the aged sweep reclaims it, so a process
    // that exits inside that window leaves thousands of them for the next open to collect —
    // routine, and one line per file would be thousands of lines of startup noise on a
    // healthy store. (It is also a hazard: `evald query` writes that to stderr, and a
    // caller that does not drain the pipe until the child exits deadlocks against it.)
    let mut removed_blocks = 0usize;
    let mut removed_tmp = 0usize;
    let mut example: Option<String> = None;
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
                removed_tmp += 1;
            } else if name.ends_with(".parquet") {
                let rel = path
                    .strip_prefix(data_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !known.contains(rel.as_str()) {
                    fs::remove_file(&path)?;
                    removed_blocks += 1;
                    example.get_or_insert(rel);
                }
            }
        }
    }
    if removed_blocks > 0 || removed_tmp > 0 {
        tracing::info!(
            blocks = removed_blocks,
            tmp = removed_tmp,
            example = example.as_deref().unwrap_or("-"),
            "swept files under blocks/ that the index does not reference \
             (a merge's retired inputs, or a flush that crashed before its commit)"
        );
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
            // Guardrail and retention OFF by default in tests: both are timer-driven, and
            // the guardrail additionally reads the HOST's free space — so leaving them on
            // would make unrelated tests depend on the machine they run on. The tests that
            // exercise them opt in explicitly.
            retention: None,
            retention_interval: Duration::from_secs(3600),
            disk_min_free_bytes: 0,
            disk_warn_free_bytes: 0,
            disk_check_interval: None,
            redactor: None,
            rollup: crate::rollup::RollupConfig::default(),
            usage: crate::usage::UsageConfig::default(),
            // Merging OFF by default in tests, for the same reason: the compactor tick is
            // what would run it, and the merge tests drive it explicitly via `merge_now`.
            merge: MergePolicy {
                hour_threshold: 0,
                ..MergePolicy::default()
            },
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
            ..test_config()
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

    // --- disk guardrail + automatic retention ---------------------------------------

    /// Poll `f` until it holds, or fail after `label`'s deadline. Background tasks here are
    /// timer-driven, so tests wait on the *condition* rather than on a sleep long enough to
    /// "probably" be enough — which is how a suite acquires flaky tests.
    async fn until(label: &str, f: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {label}");
    }

    /// The floor refuses ingest rather than letting the WAL run the volume to zero.
    ///
    /// Setting the floor to `u64::MAX` puts any real filesystem below it, so the guardrail
    /// can be exercised without contriving a full disk — the comparison under test is
    /// `free < floor`, and which side is unreachable does not change it.
    #[tokio::test]
    async fn disk_floor_refuses_ingest_and_reports_why() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                disk_min_free_bytes: u64::MAX,
                disk_check_interval: Some(Duration::from_millis(20)),
                compact_interval: None,
                ..test_config()
            },
        )
        .unwrap();

        until("the guardrail to sample and block", || {
            store.ingest_stats().disk_blocked
        })
        .await;

        let err = store
            .append(vec![test_span("t", "01", 1_800_000_000_000_000_000)])
            .await
            .expect_err("ingest must be refused below the floor");
        // Specifically DiskFull, not Backpressure: the two mean different things to an
        // operator and map to different HTTP statuses.
        assert!(
            matches!(err, StoreError::DiskFull { .. }),
            "expected DiskFull, got {err:?}"
        );
        // The error carries what was seen and what was configured, so the 503 is actionable.
        assert!(err.to_string().contains("floor"), "{err}");

        let stats = store.ingest_stats();
        assert_eq!(stats.disk_blocked_spans, 1, "the refusal must be counted");
        assert!(stats.disk_free_bytes.is_some(), "a sample was taken");
        // The refusal is counted apart from backlog shedding — conflating them would hide
        // which of two unrelated problems is happening.
        assert_eq!(stats.rejections, 0);
        assert_eq!(stats.spans_ingested, 0, "nothing was made durable");
    }

    /// An empty batch is a no-op even below the floor.
    ///
    /// Regression: the guard refused on `disk_blocked` alone, so a zero-span export got a
    /// retryable 503 for a request that would never have touched the disk — and silently,
    /// since `disk_blocked_spans` moves by zero. `append` has always documented an empty
    /// batch as a no-op; below the floor it stopped being one.
    #[tokio::test]
    async fn an_empty_batch_is_a_no_op_even_below_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                disk_min_free_bytes: u64::MAX,
                disk_check_interval: Some(Duration::from_millis(20)),
                compact_interval: None,
                ..test_config()
            },
        )
        .unwrap();

        until("the guardrail to sample and block", || {
            store.ingest_stats().disk_blocked
        })
        .await;

        assert!(store.reject_if_disk_full(0).is_ok(), "nothing to protect");
        assert!(store.append(Vec::new()).await.is_ok(), "documented no-op");
        // One span, and the floor bites — so the case above is about emptiness, not about
        // the guardrail having quietly switched off.
        assert!(store
            .append(vec![test_span("t", "01", 1_800_000_000_000_000_000)])
            .await
            .is_err());
        assert_eq!(
            store.ingest_stats().disk_blocked_spans,
            1,
            "only the real span counts as refused"
        );
    }

    /// The guardrail must FAIL OPEN. Disabled, or unable to read free space, it may never
    /// block ingest — refusing writes because we could not measure the disk would invent an
    /// outage the disk never had.
    #[tokio::test]
    async fn disk_guardrail_disabled_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                // A floor that nothing could satisfy, but no probe to enforce it.
                disk_min_free_bytes: u64::MAX,
                disk_check_interval: None,
                compact_interval: None,
                ..test_config()
            },
        )
        .unwrap();

        // No sampler runs, so there is nothing to wait for; the state must be open now.
        let stats = store.ingest_stats();
        assert!(!stats.disk_blocked);
        assert_eq!(
            stats.disk_free_bytes, None,
            "an unsampled probe must report None, never 0 — 0 reads as 'full' on a dashboard"
        );
        store
            .append(vec![test_span("t", "01", 1_800_000_000_000_000_000)])
            .await
            .expect("ingest must be accepted when the guardrail is disabled");
    }

    /// Automatic retention drops blocks past the window on its own, with no CLI invocation.
    #[tokio::test]
    async fn automatic_retention_sweeps_old_blocks_on_a_timer() {
        let dir = tempfile::tempdir().unwrap();
        const OLD: u64 = 1_600_000_000_000_000_000; // 2020
                                                    // A window far shorter than the span's age, so the sweep must drop its block. The
                                                    // cutoff is computed from the wall clock, exactly as it is in production.
        let store = Store::open(
            dir.path(),
            StoreConfig {
                retention: Some(Duration::from_secs(1)),
                retention_interval: Duration::from_millis(50),
                compact_interval: None,
                disk_check_interval: None,
                ..test_config()
            },
        )
        .unwrap();

        store
            .append(vec![test_span("old", "01", OLD)])
            .await
            .unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        assert_eq!(store.index.all_block_paths().unwrap().len(), 1);

        // Wait on the COUNTER, not on the index being empty. The sweep drops the blocks
        // first and increments its counters afterwards, so polling the index can return the
        // instant the blocks vanish — a window in which `retention_sweeps` is still 0 and
        // asserting on it fails. (Observed once before this was tightened; waiting on the
        // last thing the task writes makes every earlier effect already visible.)
        until("the automatic sweep to record a completed pass", || {
            store.ingest_stats().retention_sweeps > 0
        })
        .await;

        assert!(
            store.index.all_block_paths().unwrap().is_empty(),
            "the aged block must be gone once a sweep has completed"
        );
        let stats = store.ingest_stats();
        assert_eq!(stats.retention_blocks_dropped, 1);
        assert!(
            stats.retention_bytes_reclaimed > 0,
            "dropping a block must reclaim its bytes"
        );
    }

    /// Retention left unset must never delete anything — the default cannot be destructive.
    #[tokio::test]
    async fn retention_unset_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        const OLD: u64 = 1_600_000_000_000_000_000;
        let store = Store::open(
            dir.path(),
            StoreConfig {
                retention: None,
                retention_interval: Duration::from_millis(20),
                compact_interval: None,
                disk_check_interval: None,
                ..test_config()
            },
        )
        .unwrap();
        store
            .append(vec![test_span("old", "01", OLD)])
            .await
            .unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();

        // Long enough that a sweep would have fired many times over had one been scheduled.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            store.index.all_block_paths().unwrap().len(),
            1,
            "a 2020 span must survive indefinitely when no retention window is set"
        );
        assert_eq!(store.ingest_stats().retention_sweeps, 0);
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

    /// The dedup key must be exact: two different spans never share one, and one span keyed
    /// twice always matches itself. A hash would be smaller and would put a silent
    /// wrong-answer on the read path; this pins that the compact form cannot.
    #[test]
    fn the_dedup_key_separates_every_distinct_identity() {
        let key = |t: &str, s: &str| span_key(&test_span(t, s, 0));
        let canonical = ("0123456789abcdef0123456789abcdef", "fedcba9876543210");

        // Canonical ids take the compact form and round-trip.
        assert_eq!(key(canonical.0, canonical.1), key(canonical.0, canonical.1));
        assert!(matches!(key(canonical.0, canonical.1), SpanKey::Otlp(_)));

        // One bit of difference anywhere is a different key.
        assert_ne!(
            key(canonical.0, canonical.1),
            key("0123456789abcdef0123456789abcdee", canonical.1)
        );
        assert_ne!(
            key(canonical.0, canonical.1),
            key(canonical.0, "fedcba9876543211")
        );

        // Ids that are not canonical hex keep their exact text, and cannot be confused with
        // each other however the two halves are split — the failure a joined string has.
        for (a, b) in [
            (("t-alpha", "a1"), ("t-alpha", "a2")),
            (("a\0b", ""), ("a", "b")),
            (("", "ab"), ("ab", "")),
            // Uppercase is not canonical, so it takes the other variant — and must not
            // collide with the lowercase form's compact key.
            (
                (canonical.0.to_uppercase().as_str(), canonical.1),
                canonical,
            ),
        ] {
            assert_ne!(key(a.0, a.1), key(b.0, b.1), "{a:?} vs {b:?}");
        }

        // Wrong-length hex is not canonical either, and is still kept exactly.
        assert!(matches!(key("abcd", "ef01"), SpanKey::Other(_)));
        assert_ne!(key("abcd", "ef01"), key("abcdef01", ""));
    }

    /// A bounded read must return exactly what an unbounded read's prefix would.
    ///
    /// This is the property the top-`limit` selection has to preserve, and it is stronger
    /// than checking a hand-written expected list: it pins the newest-first order, the
    /// tie-break, and the hot/cold dedup all at once, against the path that does the naive
    /// thing. Ties are deliberate — several spans share a `start_unix_nano`, which is where
    /// a heap most easily disagrees with a stable sort.
    #[tokio::test]
    async fn a_bounded_query_returns_the_same_rows_as_the_unbounded_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;

        // Cold: 20 spans over 10 distinct start times, so every time is a tie.
        for i in 0..20u64 {
            store
                .append(vec![test_span(
                    &format!("c{i}"),
                    &format!("s{i}"),
                    1_000 + i / 2,
                )])
                .await
                .unwrap();
        }
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
        // Hot: 20 more, interleaved onto the same start times.
        for i in 0..20u64 {
            store
                .append(vec![test_span(
                    &format!("h{i}"),
                    &format!("t{i}"),
                    1_000 + i / 2,
                )])
                .await
                .unwrap();
        }

        let all = store.query(None, usize::MAX).unwrap();
        assert_eq!(all.len(), 40);
        for limit in [1usize, 2, 7, 20, 39, 40, 41, 1000] {
            let bounded = store.query(None, limit).unwrap();
            let want = &all[..limit.min(all.len())];
            assert_eq!(
                bounded.len(),
                want.len(),
                "limit {limit} returns the right count"
            );
            let got_keys: Vec<_> = bounded.iter().map(span_key).collect();
            let want_keys: Vec<_> = want.iter().map(span_key).collect();
            assert_eq!(got_keys, want_keys, "limit {limit} matches the prefix");
        }
        assert!(
            store.query(None, 0).unwrap().is_empty(),
            "a limit of 0 reads nothing"
        );

        // And the same for a trace-filtered read, which takes the same path.
        let one = store.query(Some("h3"), 10).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].span_id, "t3");
    }

    /// A span present in BOTH tiers is returned once, and the hot copy is the one returned —
    /// including when the bounded selection is what decides which rows survive.
    #[tokio::test]
    async fn a_span_in_both_tiers_is_returned_once_and_hot_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path()).await;

        let mut cold_copy = test_span("dup", "d1", 5_000);
        cold_copy.name = "cold".into();
        store.append(vec![cold_copy]).await.unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();

        // The same (trace_id, span_id) re-appears in the hot tier — the shape a re-flush
        // after a crash window leaves behind — plus two newer spans.
        let mut hot_copy = test_span("dup", "d1", 5_000);
        hot_copy.name = "hot".into();
        store.append(vec![hot_copy]).await.unwrap();
        store
            .append(vec![
                test_span("n1", "x1", 9_000),
                test_span("n2", "x2", 9_001),
            ])
            .await
            .unwrap();

        let all = store.query(None, usize::MAX).unwrap();
        assert_eq!(all.len(), 3, "the duplicate is collapsed: {all:?}");
        let dup: Vec<_> = all.iter().filter(|s| s.span_id == "d1").collect();
        assert_eq!(dup.len(), 1, "exactly one copy survives");
        assert_eq!(dup[0].name, "hot", "the hot copy wins");

        // With a bound that excludes it, it is simply absent — not duplicated, and not
        // resurrected from cold because the hot copy lost the selection.
        let top2 = store.query(None, 2).unwrap();
        assert_eq!(top2.len(), 2);
        assert!(
            top2.iter().all(|s| s.span_id != "d1"),
            "the older duplicate is out of the top 2: {top2:?}"
        );
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
                ..test_config()
            },
        )
        .unwrap();
        store.append(vec![test_span("aa", "01", 10)]).await.unwrap();
        store.seal_now().await.unwrap(); // makes the segment eligible

        // The background compactor (not compact_now) should pick it up within a few ticks. Poll for
        // up to 5 s: at 1 s this failed whenever the machine was busy (a parallel build, a
        // benchmark), while passing alone every time; the assertion below is what matters.
        for _ in 0..250 {
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

    // --- cold-to-cold merging (C13a) ----------------------------------------------------

    /// Flush `n` two-span segments into cold, one block each, with spans starting at
    /// `base_start` (so they share a partition when the starts do). Returns the trace ids.
    async fn flush_n_blocks(store: &Store, n: usize, base_start: u64) -> Vec<String> {
        let mut traces = Vec::new();
        for i in 0..n {
            let trace = format!("t{i:02}-{base_start}");
            let start = base_start + i as u64 * 2;
            store
                .append(vec![
                    test_span(&trace, "01", start),
                    test_span(&trace, "02", start + 1),
                ])
                .await
                .unwrap();
            store.seal_now().await.unwrap();
            store.compact_now().await.unwrap();
            traces.push(trace);
        }
        traces
    }

    fn merge_config(policy: MergePolicy) -> StoreConfig {
        StoreConfig {
            merge: policy,
            ..test_config()
        }
    }

    /// 2023-11-14T22:00Z — a closed UTC day, the partition the integration harness uses.
    const CLOSED_DAY_HOUR_22: u64 = 1_700_000_000_000_000_000;

    #[tokio::test]
    async fn hour_merge_collapses_flush_blocks_and_serves_every_span() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), test_config()).unwrap();
        let traces = flush_n_blocks(&store, 5, CLOSED_DAY_HOUR_22).await;
        assert_eq!(store.cold_block_count().unwrap(), 5);
        let inputs = store.index.all_block_paths().unwrap();

        // A dry run plans the same merge and changes nothing.
        let dry = store.merge_now(MergeScope::Full, true).await.unwrap();
        assert_eq!(
            (dry.merges, dry.blocks_in, dry.spans, dry.dry_run),
            (1, 5, 10, true)
        );
        assert_eq!(store.cold_block_count().unwrap(), 5);
        assert_eq!(
            store.ingest_stats().cold_merges,
            0,
            "a preview is not counted"
        );

        let report = store.merge_now(MergeScope::Full, false).await.unwrap();
        assert_eq!(
            (report.merges, report.blocks_in, report.blocks_out),
            (1, 5, 1)
        );
        assert_eq!(report.spans, 10);
        assert!(report.bytes_in > 0 && report.bytes_out > 0);

        // One block now, named by the seqno range it covers, in the same hour partition.
        let rels = store.index.all_block_paths().unwrap();
        assert_eq!(
            rels,
            vec!["blocks/2023/11/14/22/merged-00000000000000000001-00000000000000000005.parquet"]
        );
        // The watermark is a flush concept; a merge never moves it.
        assert_eq!(store.index.watermark().unwrap(), 5);

        // Reads: every span, by trace and unfiltered, from the merged block.
        assert_eq!(store.query(None, 100).unwrap().len(), 10);
        for t in &traces {
            assert_eq!(
                store.trace(t).unwrap().len(),
                2,
                "trace {t} follows the merge"
            );
        }

        // Provenance in the Parquet footer names the inputs, so a mirror of this directory
        // (the fleet uploader) can retire them.
        let (prov, rows) = cold::read_provenance(&dir.path().join(&rels[0])).unwrap();
        assert_eq!(prov.kind, cold::BlockKind::Merged);
        assert_eq!((prov.seqno_lo, prov.seqno_hi, rows), (1, 5, 10));
        assert_eq!(prov.merged_from, inputs);
        assert!(prov.declared);
        // Every input was flushed by this process, so the merge names its one table.
        assert_eq!(
            prov.price_tables,
            vec![crate::price::current().version.clone()]
        );

        // The inputs are unreferenced but still on disk (a query that listed them a moment
        // ago may be reading them); the aged sweep reclaims them.
        for rel in &inputs {
            assert!(
                dir.path().join(rel).exists(),
                "{rel} kept for the grace period"
            );
        }
        assert_eq!(
            store
                .sweep_unindexed(Duration::from_secs(3600))
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.sweep_unindexed(Duration::ZERO).await.unwrap(), 5);
        for rel in &inputs {
            assert!(!dir.path().join(rel).exists(), "{rel} reclaimed");
        }
        assert!(dir.path().join(&rels[0]).exists());

        let stats = store.ingest_stats();
        assert_eq!(
            (
                stats.cold_blocks,
                stats.cold_merges,
                stats.cold_blocks_merged
            ),
            (1, 1, 5)
        );
        assert_eq!(stats.cold_merge_failures, 0);

        // Nothing to merge now: a single block is not a merge.
        let again = store.merge_now(MergeScope::Full, false).await.unwrap();
        assert!(again.is_noop());

        // Survives reopen: the index names the merged block, the sweep finds no orphans.
        drop(store);
        let store = Store::open(dir.path(), test_config()).unwrap();
        assert_eq!(store.cold_block_count().unwrap(), 1);
        assert_eq!(store.query(None, 100).unwrap().len(), 10);
        assert_eq!(store.trace(&traces[2]).unwrap().len(), 2);
    }

    /// The unlink grace must run from the moment a block leaves the index, not from when it
    /// was written — otherwise every input older than the grace is eligible for deletion in
    /// the same pass that retires it, and the window protecting an in-flight reader is zero
    /// for exactly the blocks a long scan is most likely to still be reading.
    ///
    /// The other merge tests cannot catch this: their blocks are seconds old, so a
    /// write-time grace and a retirement-time grace agree. This one backdates them first.
    #[tokio::test]
    async fn an_old_block_still_gets_its_full_grace_after_a_merge_retires_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), test_config()).unwrap();
        flush_n_blocks(&store, 3, CLOSED_DAY_HOUR_22).await;

        // Age the inputs well past the grace, as any block in a real store would be.
        let inputs = store.index.all_block_paths().unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(7_200);
        for rel in &inputs {
            let f = fs::File::options()
                .write(true)
                .open(dir.path().join(rel))
                .unwrap();
            f.set_times(fs::FileTimes::new().set_modified(two_hours_ago))
                .unwrap();
        }

        store.merge_now(MergeScope::Full, false).await.unwrap();

        // Retired, but every one of them is still readable for the grace window.
        assert_eq!(
            store
                .sweep_unindexed(Duration::from_secs(3_600))
                .await
                .unwrap(),
            0,
            "a one-hour grace must protect inputs retired seconds ago, however old the files"
        );
        for rel in &inputs {
            assert!(dir.path().join(rel).exists(), "{rel} kept for its grace");
        }
        assert_eq!(store.query(None, 100).unwrap().len(), 6);

        // And once the grace has elapsed they go.
        assert_eq!(store.sweep_unindexed(Duration::ZERO).await.unwrap(), 3);
    }

    /// The retirement stamp is applied *before* the index commit, so there is no instant at
    /// which an input is unreferenced while still carrying its original mtime. If the stamp
    /// cannot be applied the merge must abandon rather than retire — otherwise the very next
    /// sweep deletes an old input with no grace at all.
    ///
    /// Forcing a real `set_times` failure portably is not practical, so this pins the half
    /// that makes the abort possible: `mark_retired` surfaces the error rather than reporting
    /// success. `merge_chunk` propagates it with `?` before `commit_merge` runs.
    #[tokio::test]
    async fn a_retirement_stamp_that_cannot_be_applied_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("blocks/2026/01/01/00/nope.parquet");
        let e =
            super::merge::mark_retired(&missing).expect_err("a missing block cannot be stamped");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn merged_blocks_are_row_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            merge_config(MergePolicy {
                hour_threshold: 0,
                max_spans_per_block: 4, // two flush blocks of two spans each
                ..MergePolicy::default()
            }),
        )
        .unwrap();
        flush_n_blocks(&store, 5, CLOSED_DAY_HOUR_22).await;
        let report = store.merge_now(MergeScope::Full, false).await.unwrap();
        // 5 blocks × 2 spans under a cap of 4: [1,2] [3,4] merge, 5 stays as it is.
        assert_eq!((report.merges, report.blocks_in), (2, 4));
        let rels = store.index.all_block_paths().unwrap();
        assert_eq!(rels.len(), 3);
        assert!(rels
            .iter()
            .any(|r| r.ends_with("merged-00000000000000000001-00000000000000000002.parquet")));
        assert!(rels
            .iter()
            .any(|r| r.ends_with("merged-00000000000000000003-00000000000000000004.parquet")));
        assert!(rels
            .iter()
            .any(|r| r.ends_with("00000000000000000005-0.parquet")));
        assert_eq!(store.query(None, 100).unwrap().len(), 10);
        // A block at the cap is never an input again: a second pass has nothing to do.
        assert!(store
            .merge_now(MergeScope::Full, false)
            .await
            .unwrap()
            .is_noop());
    }

    #[tokio::test]
    async fn the_compactor_tick_merges_at_the_hour_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: Some(Duration::from_millis(50)),
                merge: MergePolicy {
                    hour_threshold: 3,
                    unlink_grace: Duration::ZERO,
                    ..MergePolicy::default()
                },
                ..test_config()
            },
        )
        .unwrap();
        let inputs_before = {
            flush_n_blocks(&store, 2, CLOSED_DAY_HOUR_22).await;
            // Two blocks: under the threshold, so a tick leaves them alone.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let rels = store.index.all_block_paths().unwrap();
            assert_eq!(rels.len(), 2, "below the threshold nothing merges");
            rels
        };
        flush_n_blocks(&store, 1, CLOSED_DAY_HOUR_22 + 1_000).await;

        // The third block makes the hour due; the next tick merges and, with no grace,
        // reclaims the inputs in the same pass.
        until("tick merged the hour partition", || {
            store.cold_block_count().unwrap() == 1
        })
        .await;
        until("inputs reclaimed", || {
            inputs_before.iter().all(|r| !dir.path().join(r).exists())
        })
        .await;
        // The counters are recorded once the blocking pass returns, a moment after its
        // files are visible — so wait on them too rather than read them racily.
        until("merge counted", || store.ingest_stats().cold_merges >= 1).await;
        let stats = store.ingest_stats();
        assert_eq!((stats.cold_merges, stats.cold_blocks_merged), (1, 3));
        assert_eq!(stats.cold_merge_failures, 0);
        assert_eq!(store.query(None, 100).unwrap().len(), 6);
    }

    #[tokio::test]
    async fn closed_days_collapse_once_quiet_and_the_current_day_only_by_hour() {
        let dir = tempfile::tempdir().unwrap();
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        {
            let store = Store::open(
                dir.path(),
                merge_config(MergePolicy {
                    hour_threshold: 2,
                    day_quiet: Duration::ZERO,
                    ..MergePolicy::default()
                }),
            )
            .unwrap();
            // A closed day with one block in each of two hours (under the hour threshold),
            // and the current hour with two blocks (at it).
            flush_n_blocks(&store, 1, CLOSED_DAY_HOUR_22).await;
            flush_n_blocks(&store, 1, CLOSED_DAY_HOUR_22 + 3_600 * 1_000_000_000).await;
            flush_n_blocks(&store, 2, now_nanos).await;
            assert_eq!(store.cold_block_count().unwrap(), 4);

            let report = store.merge_now(MergeScope::Tick, false).await.unwrap();
            assert_eq!((report.merges, report.blocks_in), (2, 4));
            let rels = store.index.all_block_paths().unwrap();
            assert_eq!(rels.len(), 2);
            let today = cold::partition_of(now_nanos);
            assert!(
                rels.contains(
                    &"blocks/2023/11/14/day-00000000000000000001-00000000000000000002.parquet"
                        .to_string()
                ),
                "closed day collapsed into a day block: {rels:?}"
            );
            assert!(
                rels.iter()
                    .any(|r| r.starts_with(&format!("blocks/{today}/merged-"))),
                "the current hour merged in place, never into a day block: {rels:?}"
            );
            assert_eq!(store.query(None, 100).unwrap().len(), 8);
        }

        // A closed day that is still being written into (a backfill, a skewed clock) is
        // not rewritten on every tick: the tick waits for it to go quiet; `evald compact`
        // (Full) does not.
        let store = Store::open(
            dir.path(),
            merge_config(MergePolicy {
                hour_threshold: 2,
                day_quiet: Duration::from_secs(3600),
                ..MergePolicy::default()
            }),
        )
        .unwrap();
        // (The reopen replays the empty active segment 5, so this flush is seqno 6.)
        flush_n_blocks(&store, 1, CLOSED_DAY_HOUR_22 + 10).await;
        assert_eq!(store.cold_block_count().unwrap(), 3);
        assert!(store
            .merge_now(MergeScope::Tick, false)
            .await
            .unwrap()
            .is_noop());
        let full = store.merge_now(MergeScope::Full, false).await.unwrap();
        assert_eq!((full.merges, full.blocks_in), (1, 2));
        let rels = store.index.all_block_paths().unwrap();
        assert!(
            rels.contains(
                &"blocks/2023/11/14/day-00000000000000000001-00000000000000000006.parquet"
                    .to_string()
            ),
            "{rels:?}"
        );
        assert_eq!(store.query(None, 100).unwrap().len(), 10);
    }

    #[tokio::test]
    async fn a_merge_that_crashed_before_its_commit_leaves_the_inputs_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let out = "blocks/2023/11/14/22/merged-00000000000000000001-00000000000000000003.parquet";
        {
            let store = Store::open(dir.path(), test_config()).unwrap();
            flush_n_blocks(&store, 3, CLOSED_DAY_HOUR_22).await;
            // Simulate a crash after the output was renamed into place but before the
            // redb swap: a complete-looking block the index does not name.
            let input = dir
                .path()
                .join("blocks/2023/11/14/22/00000000000000000001-0.parquet");
            fs::copy(&input, dir.path().join(out)).unwrap();
            fs::write(
                dir.path().join("blocks/2023/11/14/22/merged-x.parquet.tmp"),
                b"partial",
            )
            .unwrap();
        }
        // Open sweeps the un-indexed output and the tmp; the inputs (still indexed) stay.
        let store = Store::open(dir.path(), test_config()).unwrap();
        assert!(
            !dir.path().join(out).exists(),
            "uncommitted merge output swept"
        );
        assert!(!dir
            .path()
            .join("blocks/2023/11/14/22/merged-x.parquet.tmp")
            .exists());
        assert_eq!(store.cold_block_count().unwrap(), 3);
        assert_eq!(store.query(None, 100).unwrap().len(), 6);
        // The merge simply runs again and lands on the same name.
        let report = store.merge_now(MergeScope::Full, false).await.unwrap();
        assert_eq!(report.blocks_in, 3);
        assert_eq!(
            store.index.all_block_paths().unwrap(),
            vec![out.to_string()]
        );
        assert_eq!(store.query(None, 100).unwrap().len(), 6);
    }
}
