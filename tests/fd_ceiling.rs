//! The open-file ceiling on the cold read path (`docs/SOAK.md`, "The ceiling this gate found").
//!
//! The first full-length soak failed with `EMFILE` while planning `SELECT COUNT(*) FROM spans`
//! over ~18,600 committed blocks. Two things caused it, and this file pins both fixes:
//!
//! 1. **Unbounded concurrent opens.** Every block is registered as its own listing URL, and
//!    planning resolved them all at once — one `open` per block, in flight together. A
//!    concurrency-limited object store now caps that at [`evald::sql::COLD_SCAN_MAX_OPEN_FILES`],
//!    so the descriptors a scan needs no longer track the block count.
//! 2. **An unbounded block count.** Nothing merged cold blocks, so the count only grew.
//!    `store::merge` collapses them (C13a).
//!
//! Test 1 asserts a query over more blocks than the process may have descriptors still
//! succeeds; test 2 asserts merging collapses what a scan has to open at all. Unix-only:
//! the limit is applied with `ulimit -n` in a shell, which is the thing under test.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use evald::model::{Dialect, Tokens};
use evald::store::{MergePolicy, MergeScope};
use evald::{NormalizedSpan, Store, StoreConfig};

/// Enough blocks that the pre-fix behaviour cannot fit inside `NOFILE`. Measured on the
/// unbounded path: 600 blocks exhausted a 256-descriptor limit, so this sits above it.
const BLOCKS: u64 = 800;
/// Far below `BLOCKS`, comfortably above the scan's own cap plus the runtime's handles
/// (stdio, the redb files, the tokio machinery).
const NOFILE: u32 = 192;

/// A minimal span: identity and timestamps only — the partition and the dedupe key.
fn span(trace_id: &str, span_id: &str, start_unix_nano: u64) -> NormalizedSpan {
    NormalizedSpan {
        dialect: Dialect::Unknown,
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        name: "s".to_string(),
        otel_kind: 1,
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

/// Merging off, so the store keeps one block per flush — the shape the soak hit the ceiling in.
fn unmerged_config() -> StoreConfig {
    StoreConfig {
        compact_interval: None,
        merge: MergePolicy {
            hour_threshold: 0,
            ..MergePolicy::default()
        },
        ..StoreConfig::default()
    }
}

/// Hour partitions the blocks are spread across — all inside one closed UTC day, so the
/// day phase has exactly one day to collapse.
const HOURS: u64 = 4;
/// 2023-11-14T00:13:20Z. Early enough in the day that `HOURS` more hours stay inside it.
const BASE_UNIX_NANO: u64 = 1_699_920_800_000_000_000;

/// `blocks` one-span blocks spread over [`HOURS`] hour partitions of one closed UTC day.
async fn build_store(dir: &Path, blocks: u64) -> Store {
    let store = Store::open(dir, unmerged_config()).unwrap();
    for i in 0..blocks {
        let start = BASE_UNIX_NANO + (i % HOURS) * 3_600_000_000_000 + i;
        let s = span(&format!("{i:032x}"), &format!("{i:016x}"), start);
        store.append(vec![s]).await.unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
    }
    assert_eq!(store.cold_block_count().unwrap(), blocks);
    store
}

/// Run `evald query` with the process descriptor limit lowered to `nofile`.
///
/// `ulimit` is a shell builtin, so the limit is applied by `sh` and inherited across the
/// `exec` into evald — the same way an operator's shell or a systemd `LimitNOFILE=` would.
fn query_under_ulimit(data_dir: &Path, nofile: u32, sql: &str) -> (bool, String, String) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -n {nofile} && exec \"$0\" query \"$1\" --data-dir \"$2\" --limit 10"
        ))
        .arg(env!("CARGO_BIN_EXE_evald"))
        .arg(sql)
        .arg(data_dir)
        .output()
        .expect("spawn evald query under a lowered descriptor limit");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The count a `SELECT COUNT(*)` returned, parsed out of `evald query`'s JSON rows.
fn count_from(stdout: &str) -> i64 {
    let v: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("query output is not JSON ({e}): {stdout}"));
    v[0]["n"]
        .as_i64()
        .unwrap_or_else(|| panic!("no count in query output: {stdout}"))
}

/// A full scan over more blocks than the process has descriptors still answers.
///
/// This is the soak's `EMFILE` as a unit-sized regression test: before the scan's opens were
/// bounded, planning opened every block at once and this died with "Too many open files".
#[tokio::test]
async fn a_full_scan_survives_more_blocks_than_the_descriptor_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = build_store(dir.path(), BLOCKS).await;
    drop(store); // redb is exclusive: the CLI needs the data-dir to itself.

    let (ok, stdout, stderr) =
        query_under_ulimit(dir.path(), NOFILE, "SELECT COUNT(*) AS n FROM spans");
    assert!(
        ok,
        "a scan over {BLOCKS} blocks under `ulimit -n {NOFILE}` must not exhaust descriptors\n{stderr}"
    );
    assert_eq!(count_from(&stdout), BLOCKS as i64, "every span is scanned");

    // A filtered scan takes the same path (the `spans` view is an anti-join over cold), so it
    // must hold there too — that is the query shape the soak was running.
    let (ok, stdout, stderr) = query_under_ulimit(
        dir.path(),
        NOFILE,
        "SELECT COUNT(*) AS n FROM spans WHERE trace_id = '00000000000000000000000000000007'",
    );
    assert!(ok, "filtered scan under the same limit\n{stderr}");
    assert_eq!(count_from(&stdout), 1);
}

/// Merging bounds the block count itself, which is the durable half of the fix: the scan's
/// cap keeps a query alive, merging keeps the file set small in the first place.
#[tokio::test]
async fn merging_collapses_the_block_count_a_scan_opens() {
    let dir = tempfile::tempdir().unwrap();
    let store = build_store(dir.path(), BLOCKS).await;

    // Two phases in one pass: each hour partition collapses to one block, then the closed
    // day collapses those into one. So the pass consumes BLOCKS + HOURS and rewrites each
    // span twice — the bounded write amplification the design allows for.
    let report = store.merge_now(MergeScope::Full, false).await.unwrap();
    assert_eq!(report.blocks_in, (BLOCKS + HOURS) as usize);
    assert_eq!(report.spans, BLOCKS * 2);
    assert_eq!(
        store.cold_block_count().unwrap(),
        1,
        "a closed day of {BLOCKS} blocks ends as one"
    );
    // Nothing is left behind: the inputs leave the index and the sweep reclaims them.
    assert_eq!(
        store
            .sweep_unindexed(std::time::Duration::ZERO)
            .await
            .unwrap(),
        (BLOCKS + HOURS) as usize
    );
    drop(store);

    let (ok, stdout, stderr) =
        query_under_ulimit(dir.path(), NOFILE, "SELECT COUNT(*) AS n FROM spans");
    assert!(ok, "scan of the merged store\n{stderr}");
    assert_eq!(
        count_from(&stdout),
        BLOCKS as i64,
        "a merge preserves every span"
    );
}
