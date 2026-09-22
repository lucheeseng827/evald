//! What the shipped hot tier actually costs — the measurements behind the engine decision
//! in `HOT_TIER_DECISION.md`.
//!
//! `PLAN.md` §6 gated the engine choice on data: *"benchmark the LSM vs a plain
//! segmented-WAL-of-Arrow-batches before committing — for an append-mostly,
//! query-elsewhere workload we may not need an LSM at all"*. An LSM hot tier would buy
//! three things over an in-memory one: spill to disk so memory is not the bound, bounded
//! recovery instead of a replay proportional to the backlog, and indexed reads over the
//! un-compacted set. This file measures all three on the shipped engine, so the question is
//! whether those are problems worth a second on-disk format.
//!
//! Run it:
//!
//! ```bash
//! cargo test --release --test hot_tier_bounds -- --ignored --nocapture
//! ```
//!
//! Release matters: a debug build measures `rustc`, not the design. The numbers in the
//! decision record were taken this way and say which machine produced them.
//!
//! The one **assertion** here (not ignored, runs in CI) is the property the measurements
//! exist to support: the hot tier is bounded, and the bound is enforced by shedding rather
//! than by hoping the compactor keeps up.

use std::time::{Duration, Instant};

use evald::model::{Dialect, Tokens};
use evald::store::MergePolicy;
use evald::{NormalizedSpan, Store, StoreConfig, StoreError};

/// An LLM span of roughly the shape the store actually holds: a model, token counts, and a
/// prompt/completion pair. Payload sizes are stated rather than guessed at — the hot tier's
/// memory cost is dominated by these two strings, so a measurement over empty spans would
/// flatter the design.
fn llm_span(n: u64, prompt_bytes: usize, completion_bytes: usize) -> NormalizedSpan {
    NormalizedSpan {
        dialect: Dialect::OpenInference,
        trace_id: format!("{n:032x}"),
        span_id: format!("{n:016x}"),
        parent_span_id: None,
        name: "chat.completion".to_string(),
        otel_kind: 3,
        oi_kind: Some("LLM".to_string()),
        start_unix_nano: 1_700_000_000_000_000_000 + n,
        end_unix_nano: 1_700_000_000_000_000_000 + n + 250_000_000,
        status_code: 0,
        status_message: None,
        model: Some("gpt-4o".to_string()),
        provider: Some("openai".to_string()),
        tokens: Tokens {
            prompt: Some(820),
            completion: Some(140),
            total: Some(960),
            ..Tokens::default()
        },
        cost_usd: Some(0.004_1),
        input_value: Some("p".repeat(prompt_bytes)),
        output_value: Some("c".repeat(completion_bytes)),
        session_id: Some(format!("sess-{}", n % 1000)),
        user_id: Some(format!("user-{}", n % 5000)),
        service_name: Some("checkout-agent".to_string()),
        scope_name: Some("openinference.instrumentation".to_string()),
        scope_version: Some("0.1.0".to_string()),
        raw_attributes: Default::default(),
    }
}

/// Resident set size in KiB, or `None` where the kernel does not publish it.
fn rss_kib() -> Option<u64> {
    proc_status_kib("VmRSS:")
}

/// Peak resident set size the process has ever reached, in KiB. Watching this across an
/// operation is how a transient allocation shows up at all: it is gone by the time the
/// operation returns, so `VmRSS` after the fact reports nothing.
fn peak_rss_kib() -> Option<u64> {
    proc_status_kib("VmHWM:")
}

fn proc_status_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix(field))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Ingest `n` spans with compaction OFF, so every one of them stays in the hot tier.
async fn fill_hot(store: &Store, n: u64, prompt: usize, completion: usize) {
    const BATCH: u64 = 500;
    let mut sent = 0;
    while sent < n {
        let batch: Vec<NormalizedSpan> = (sent..(sent + BATCH).min(n))
            .map(|i| llm_span(i, prompt, completion))
            .collect();
        sent += batch.len() as u64;
        store.append(batch).await.expect("append");
    }
}

/// Compaction and merging off, hot tier unbounded: the configuration that lets the hot
/// tier be measured at and past the shipped bound instead of shedding at it.
fn unbounded_config() -> StoreConfig {
    StoreConfig {
        compact_interval: None,
        max_hot_spans: 0,
        // Payloads stay inline: offloading them to the blob store would move the bytes
        // this measurement is about out of the hot tier and onto disk.
        blob_offload_bytes: 0,
        merge: MergePolicy {
            hour_threshold: 0,
            ..MergePolicy::default()
        },
        ..StoreConfig::default()
    }
}

#[tokio::test]
#[ignore = "measurement, not an assertion: run with --release --nocapture"]
async fn measure_hot_tier_cost() {
    // ~1 KiB of prompt + completion per span. Stated, not assumed: the point of the numbers
    // is that a reader can scale them to their own payloads.
    const PROMPT: usize = 800;
    const COMPLETION: usize = 200;

    // ONE size per process, selected by `HOT_TIER_SPANS`. Measuring several in a loop looks
    // tidier and lies: the allocator does not return freed pages to the OS between
    // iterations, so a later, larger run reports a *smaller* RSS delta than an earlier
    // smaller one. A fresh process per size is the only honest way to read RSS.
    let n: u64 = std::env::var("HOT_TIER_SPANS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);

    let dir = tempfile::tempdir().unwrap();
    let before = rss_kib();
    let store = Store::open(dir.path(), unbounded_config()).unwrap();
    fill_hot(&store, n, PROMPT, COMPLETION).await;
    assert_eq!(store.ingest_stats().hot_spans, n as usize, "all spans hot");

    let rss_delta = match (before, rss_kib()) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    };
    // A deterministic companion to RSS: what the spans themselves occupy, counted field by
    // field. RSS includes allocator slack, the runtime and the WAL's buffers; this does not,
    // so the two bracket the real cost rather than either standing alone.
    let structural: u64 = store.hot_spans().iter().map(span_footprint).sum();
    let wal_bytes = store.wal_bytes();

    // A trace lookup over the un-compacted set — the read an LSM would index.
    let target = format!("{:032x}", n / 2);
    let t = Instant::now();
    let found = store.trace(&target).unwrap();
    let lookup = t.elapsed();
    assert_eq!(found.len(), 1, "the span is served from the hot tier");

    // A bounded full scan of the hot tier — `GET /v1/spans?limit=100`, in effect. The peak
    // measured across it is what says whether the read path allocates proportional to the
    // tier: collecting every match and truncating afterwards shows up here as a second copy
    // of the hot tier, and the bounded selection that replaced it shows up as nothing.
    let peak_before = peak_rss_kib();
    let t = Instant::now();
    let rows = store.query(None, 100).unwrap();
    let scan = t.elapsed();
    assert_eq!(rows.len(), 100);
    let scan_peak = match (peak_before, peak_rss_kib()) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    };

    // The unbounded read (`trace`, `span_count`) over the same tier, which cannot be
    // bounded and so is the case a bounded selection must not make worse.
    let t = Instant::now();
    let counted = store.span_count().unwrap();
    let count_all = t.elapsed();
    assert_eq!(counted, n as usize);

    // Restart: the hot tier is rebuilt by replaying the WAL above the watermark.
    drop(store);
    let t = Instant::now();
    let store = Store::open(dir.path(), unbounded_config()).unwrap();
    let replay = t.elapsed();
    assert_eq!(store.ingest_stats().hot_spans, n as usize, "replayed whole");

    println!(
        "\nhot tier at {n} un-compacted spans ({PROMPT}B prompt + {COMPLETION}B completion, \
         payloads inline, compaction off)"
    );
    println!(
        "  RSS delta        {}",
        rss_delta.map(mib).unwrap_or_else(|| "(unavailable)".into())
    );
    println!(
        "  bytes/span RSS   {}",
        rss_delta
            .map(|kib| format!("{}", (kib * 1024) / n))
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "  span footprint   {} ({} bytes/span)",
        mib(structural / 1024),
        structural / n
    );
    println!("  WAL on disk      {}", mib(wal_bytes / 1024));
    println!("  replay on open   {}", secs(replay));
    println!("  trace lookup     {}", micros(lookup));
    println!(
        "  bounded scan     {} (peak RSS growth {})",
        secs(scan),
        scan_peak.map(mib).unwrap_or_else(|| "-".into())
    );
    println!("  unbounded scan   {}\n", secs(count_all));
}

/// Heap bytes one span holds: the struct itself plus everything it owns. Deterministic,
/// unlike RSS — and it is the number that scales with payload size, which is the knob a
/// reader actually has.
fn span_footprint(s: &NormalizedSpan) -> u64 {
    let opt = |o: &Option<String>| o.as_ref().map(|v| v.len()).unwrap_or(0);
    let attrs: usize = s
        .raw_attributes
        .iter()
        .map(|(k, v)| k.len() + v.to_string().len())
        .sum();
    (std::mem::size_of::<NormalizedSpan>()
        + s.trace_id.len()
        + s.span_id.len()
        + s.name.len()
        + opt(&s.parent_span_id)
        + opt(&s.oi_kind)
        + opt(&s.status_message)
        + opt(&s.model)
        + opt(&s.provider)
        + opt(&s.input_value)
        + opt(&s.output_value)
        + opt(&s.session_id)
        + opt(&s.user_id)
        + opt(&s.service_name)
        + opt(&s.scope_name)
        + opt(&s.scope_version)
        + attrs) as u64
}

fn mib(kib: u64) -> String {
    format!("{:.0} MiB", kib as f64 / 1024.0)
}
fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}
fn micros(d: Duration) -> String {
    format!("{} us", d.as_micros())
}

/// The property the measurements support: the hot tier is **bounded**, and the bound is
/// enforced by shedding, not by hoping the compactor keeps up.
///
/// This is the whole reason an in-memory hot tier is safe without an LSM's spill-to-disk.
/// Memory cannot grow without limit: past `max_hot_spans` the store refuses new spans with
/// backpressure, which the ingest path answers with `429 + Retry-After`. Everything already
/// accepted stays durable in the WAL — the bound is on memory, never on durability.
#[tokio::test]
async fn the_hot_tier_is_bounded_and_sheds_rather_than_growing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreConfig {
            max_hot_spans: 200,
            ..unbounded_config()
        },
    )
    .unwrap();

    // The bound is a threshold checked per batch, not a per-span cap: a batch offered while
    // the tier is under the limit is accepted whole, so the tier can overshoot by at most
    // one batch. That is the contract — bounded memory, not an exact ceiling — and it is
    // what keeps the check off the per-span path.
    fill_hot(&store, 500, 16, 16).await;
    let before = store.ingest_stats();
    assert!(
        before.shedding,
        "past the bound the store reports it: {before:?}"
    );

    // Past it, the next append is refused rather than buffered.
    let err = store
        .append(vec![llm_span(999_999, 16, 16)])
        .await
        .expect_err("past the bound, ingest sheds");
    assert!(
        matches!(err, StoreError::Backpressure),
        "shedding is backpressure (429 + Retry-After), not a silent drop: {err}"
    );
    let after = store.ingest_stats();
    assert!(after.rejections > 0, "shed spans are counted: {after:?}");
    assert_eq!(
        after.hot_spans, before.hot_spans,
        "a shed span is not held in memory anyway: {after:?}"
    );

    // Draining the backlog clears it: the bound throttles rather than jamming permanently.
    store.seal_now().await.unwrap();
    store.compact_now().await.unwrap();
    assert_eq!(store.ingest_stats().hot_spans, 0, "compaction drains it");
    store
        .append(vec![llm_span(1_000_000, 16, 16)])
        .await
        .expect("ingest resumes once the hot tier drains");
}

/// Peak memory across a **full cycle** at a candidate `--max-hot-spans`, which is the number
/// that decides how high the bound can safely go for a given container limit.
///
/// The hot tier is not the only thing competing for that limit, so measuring it alone
/// understates the answer. This drives everything that allocates against a full tier, in the
/// order a busy node hits them:
///
/// 1. the tier filled to the bound;
/// 2. a bounded read at that size;
/// 3. compaction — which clones one sealed segment at a time and builds an Arrow batch from
///    it, so it has a transient of its own that scales with `--seal-threshold`, not with the
///    bound;
/// 4. a cold-to-cold merge;
/// 5. a SQL aggregate over the blocks that produced, which is where DataFusion allocates.
///
/// Reported as peak growth over the process baseline. A real `evald serve` starts at about
/// 114 MiB (`ee/docs/FLEET-BENCHMARKS.md`), so add that to compare against a container limit.
///
/// ```bash
/// HOT_TIER_SPANS=350000 cargo test --release --test hot_tier_bounds \
///     measure_peak_under_a_full_cycle -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "measurement, not an assertion: run with --release --nocapture"]
async fn measure_peak_under_a_full_cycle() {
    const PROMPT: usize = 800;
    const COMPLETION: usize = 200;
    let n: u64 = std::env::var("HOT_TIER_SPANS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);

    let dir = tempfile::tempdir().unwrap();
    let baseline = rss_kib();
    // Merging on, at its shipped threshold: step 4 has to be the real thing.
    let store = Store::open(
        dir.path(),
        StoreConfig {
            compact_interval: None,
            max_hot_spans: 0,
            blob_offload_bytes: 0,
            ..StoreConfig::default()
        },
    )
    .unwrap();

    fill_hot(&store, n, PROMPT, COMPLETION).await;
    // Every stage below reads the PEAK, not the current RSS, so the progression is monotone
    // and each line means "the most memory held up to here". Mixing the two would charge the
    // fill's own allocator churn to whatever step came next.
    let after_fill = peak_rss_kib();

    let rows = store.query(None, 100).unwrap();
    assert_eq!(rows.len(), 100);
    let after_read = peak_rss_kib();

    // Compaction against a FULL tier — the case that stacks its transient on the bound.
    store.seal_now().await.unwrap();
    store.compact_now().await.unwrap();
    let after_flush = peak_rss_kib();
    assert_eq!(store.ingest_stats().hot_spans, 0, "the tier drained");

    store
        .merge_now(evald::store::MergeScope::Full, false)
        .await
        .unwrap();
    let after_merge = peak_rss_kib();

    let sql = evald::sql::query(
        &store,
        "SELECT model, COUNT(*) AS n, SUM(cost_usd) AS cost FROM spans GROUP BY model",
        100,
    )
    .await
    .unwrap();
    assert_eq!(sql.rows.len(), 1);
    let peak = peak_rss_kib();

    let over = |v: Option<u64>| match (baseline, v) {
        (Some(b), Some(v)) => mib(v.saturating_sub(b)),
        _ => "-".into(),
    };
    println!("\nfull cycle at --max-hot-spans {n} ({PROMPT}B + {COMPLETION}B payloads)");
    println!("  tier filled            {}", over(after_fill));
    println!("  after a bounded read   {}", over(after_read));
    println!("  after compaction       {}", over(after_flush));
    println!("  after a cold merge     {}", over(after_merge));
    println!("  after a SQL aggregate  {}", over(Some(peak.unwrap_or(0))));
    println!(
        "  PEAK over baseline     {}   (+ ~114 MiB for a real serve process)\n",
        over(peak)
    );
}
