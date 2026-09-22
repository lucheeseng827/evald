//! The C13 soak gate — `PLAN.md` §6's GA blocker, and the project's own stated single biggest
//! storage risk.
//!
//! `PLAN.md` names the gate precisely: **sustained ingest + kill-9 crash recovery +
//! compaction under load, with fsync-correctness verification**. This is that, as one test.
//! `tests/crash_recovery.rs` proves the same durability claim across ONE crash; a soak exists
//! because the failures worth fearing here are not the first crash. They are the slow ones:
//! a recovery that loses a little each time, a compaction that stops committing once it has
//! been interrupted, a dedup that starts double-counting only after the hot and cold tiers
//! have both been rebuilt a few times. Those are invisible at one cycle and obvious at ten.
//!
//! Each cycle drives concurrent ingest against a live server, waits for compaction to commit
//! new cold blocks *while that load is running*, then SIGKILLs the process mid-flight and
//! reopens the store in a fresh process. Four properties are asserted after every cycle:
//!
//! 1. **No ACK'd span is ever lost.** Every `200` is a WAL fsync, so the recovered row count
//!    must be at least the number of ACKs taken across the whole soak so far. This is the
//!    fsync-correctness verification.
//! 2. **No span is double-counted.** `COUNT(*) == COUNT(DISTINCT span_id)`, so a WAL replay
//!    that re-applies records already folded into cold Parquet shows up here.
//! 3. **Recovery never goes backwards.** The count after cycle *k* is at least the count after
//!    cycle *k-1* — a recovery that drops data it previously recovered fails, even if it is
//!    still above the ACK floor for this cycle.
//! 4. **Compaction keeps working under load, after crashes.** Every cycle must commit at least
//!    one Parquet block that did not exist when the cycle began. A store that quietly stops
//!    compacting after its first unclean shutdown still passes 1–3 while growing an unbounded
//!    WAL; this is what catches it.
//!
//! Span ids are allocated from one counter shared by every worker and every cycle, so no span
//! is ever written twice and property 2 stays meaningful across restarts.
//!
//! Ignored by default — `cargo test` must stay fast. Run it explicitly:
//!
//! ```text
//! cargo test --test soak -- --ignored --nocapture              # CI shape, ~45s
//! EVALD_SOAK_SECS=1800 EVALD_SOAK_CYCLES=20 EVALD_SOAK_WORKERS=8 EVALD_SOAK_SEAL=1000 \
//!     cargo test --release --test soak -- --ignored --nocapture   # the GA gate
//! ```

#![cfg(unix)]

mod common;

use common::{count_spans, post_span, recovered_span_ids, span_body, Server};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Read a soak knob from the environment, falling back to a value sized for CI.
fn env_u64(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{key}={v:?} is not a number: {e}")),
        Err(_) => default,
    }
}

/// Every Parquet block currently committed under `data_dir/blocks`.
///
/// Compaction may merge cold blocks, so a block *count* is not monotonic and cannot be used to
/// prove compaction ran. The set of paths can: a path that was not here at the start of this
/// cycle is a block this cycle's compaction committed.
fn block_paths(data_dir: &Path) -> HashSet<PathBuf> {
    fn walk(dir: &Path, out: &mut HashSet<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "parquet") {
                out.insert(p);
            }
        }
    }
    let mut out = HashSet::new();
    walk(&data_dir.join("blocks"), &mut out);
    out
}

#[test]
#[ignore = "soak: minutes to hours; run explicitly (see the module docs)"]
fn sustained_ingest_survives_repeated_kill9_with_compaction_under_load() {
    let bin = env!("CARGO_BIN_EXE_evald");
    let data = tempfile::tempdir().expect("tempdir");
    let logs = tempfile::tempdir().expect("tempdir for server logs");

    let total_secs = env_u64("EVALD_SOAK_SECS", 45);
    let cycles = env_u64("EVALD_SOAK_CYCLES", 3).max(1);
    let workers = env_u64("EVALD_SOAK_WORKERS", 4).max(1);
    // How many spans seal a segment. The default is tiny so a 45s CI run still commits blocks
    // continuously; the shipped default is 50000. Raise it for long runs: because cold blocks
    // are never merged, a multi-minute soak at seal=20 accumulates tens of thousands of blocks
    // and hits the cold-tier read path's file-descriptor ceiling (docs/SOAK.md, "the ceiling
    // this gate found") long before it runs out of budget — which is a real defect, but not
    // the one these four properties are here to measure.
    let seal = env_u64("EVALD_SOAK_SEAL", 20).max(1);
    let cycle_budget = Duration::from_secs_f64(total_secs as f64 / cycles as f64);

    println!(
        "soak: {total_secs}s total, {cycles} kill-9 cycles ({:.1}s each), {workers} ingest \
         workers, seal every {seal} spans",
        cycle_budget.as_secs_f64()
    );

    // One allocator for the whole soak: a span id is handed out once, ever, so a duplicate in
    // the store is the store's doing and not the test's.
    let seq = Arc::new(AtomicU64::new(0));
    // Confirmed 200s across every cycle. Each one is a WAL fsync that must survive every
    // subsequent crash, so this floor only ever rises.
    let acked = Arc::new(AtomicU64::new(0));
    // WHICH ids were ACK'd, not just how many. Counting is not enough: recovery legitimately
    // returns a few rows MORE than were ACK'd (spans fsynced just before a kill whose 200 never
    // got home), so a lost ACK'd row can be masked by one of those surplus rows and `rows >=
    // acked` still passes. Every run shows that surplus, so the masking window is real, not
    // theoretical. Ids come from one counter, so a bitset indexed by id stays compact.
    let mut acked_ids: Vec<bool> = Vec::new();

    let started = Instant::now();
    let mut prev_rows: i64 = 0;
    let mut recovered_blocks = 0usize;

    for cycle in 1..=cycles {
        let mut server = Server::start(
            bin,
            data.path(),
            logs.path().join(format!("serve.{cycle}.stderr.log")),
            // Compact every second so "compaction under load" is exercised in every cycle
            // rather than only in a long one.
            seal,
            1,
        )
        .unwrap_or_else(|e| panic!("cycle {cycle}: {e}"));
        let port = server.port;

        // What was already on disk when this cycle began. Anything beyond this set at the end
        // is a block THIS cycle committed — after the previous cycle's unclean shutdown.
        let blocks_before = block_paths(data.path());
        let acked_before = acked.load(Ordering::Relaxed);

        let stop = Arc::new(AtomicBool::new(false));
        let mut senders = Vec::new();
        for _ in 0..workers {
            let (seq, acked, stop) = (Arc::clone(&seq), Arc::clone(&acked), Arc::clone(&stop));
            senders.push(std::thread::spawn(move || {
                // Collected per worker and merged after the join, so recording an ACK costs no
                // lock on the hot path.
                let mut mine = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let id = seq.fetch_add(1, Ordering::Relaxed);
                    if post_span(port, &span_body(id)) == Some(200) {
                        acked.fetch_add(1, Ordering::Relaxed);
                        mine.push(id);
                    } else {
                        // A shed (429/503) or, once the server is gone, connection refused.
                        // Neither is an ACK; back off so a dead server is not a spin loop.
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                mine
            }));
        }

        // Hold the load until this cycle's budget is spent, but never kill before compaction has
        // actually committed something new — otherwise the cycle would assert property 4 against
        // a window too short for it to have happened, which is a flaky gate rather than a real
        // finding. Whichever takes longer wins.
        let deadline = started + cycle_budget * cycle as u32;
        // Assigned on every path that leaves the loop, so it is always initialized below.
        let new_blocks: HashSet<PathBuf>;
        loop {
            // Do NOT walk the blocks tree while the budget is still running. `block_paths` is a
            // full recursive readdir, and at a small seal threshold this tree reaches tens of
            // thousands of files — polling it during the load window puts a large, growing
            // readdir load on the same disk the store is writing to, and shows up as ingest
            // throughput that falls with block count. That is the harness measuring itself.
            // Nothing needs the set until the budget is spent, so only look then.
            if Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            let seen: HashSet<PathBuf> = block_paths(data.path())
                .difference(&blocks_before)
                .cloned()
                .collect();
            // A hard ceiling so a store that never compacts fails the assertion below instead of
            // hanging the soak forever.
            let out_of_patience = Instant::now() >= deadline + Duration::from_secs(60);
            if !seen.is_empty() || out_of_patience {
                new_blocks = seen;
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        // CRASH: SIGKILL while every worker is still posting, so the kill lands mid-flight —
        // WAL hot, cold Parquet present, very likely a flush in progress.
        server.kill9();
        stop.store(true, Ordering::Relaxed);
        for s in senders {
            for id in s.join().unwrap_or_default() {
                let idx = id as usize;
                if acked_ids.len() <= idx {
                    acked_ids.resize(idx + 1, false);
                }
                acked_ids[idx] = true;
            }
        }

        let acked_total = acked.load(Ordering::Relaxed);
        let acked_this_cycle = acked_total - acked_before;
        assert!(
            acked_this_cycle > 0,
            "cycle {cycle}: no span was ACK'd — the crash proves nothing about durability\n\
             --- evald serve stderr (tail) ---\n{}",
            server.stderr_tail()
        );

        // Property 4: compaction ran and committed under this cycle's load.
        assert!(
            !new_blocks.is_empty(),
            "cycle {cycle}: no new Parquet block was committed while {acked_this_cycle} spans \
             were ingested under load — compaction stopped after an earlier unclean shutdown\n\
             --- evald serve stderr (tail) ---\n{}",
            server.stderr_tail()
        );
        recovered_blocks += new_blocks.len();

        // RECOVER in a fresh process: WAL replay above the watermark plus the orphan-block sweep
        // for the flush the kill interrupted.
        let (rows, distinct) = count_spans(bin, data.path(), Duration::from_secs(120));

        // Property 2: no double-count.
        assert_eq!(
            rows, distinct,
            "cycle {cycle}: {distinct} distinct span_ids but {rows} rows — a span surfaced \
             more than once after recovery"
        );
        // Property 1, fast partial check: gross loss shows up as a count shortfall right here,
        // in the cycle that caused it. It is NOT sufficient on its own — recovery legitimately
        // returns a few surplus rows, so a small loss can hide behind them. The authoritative
        // identity check runs once after the last cycle, where it is equally strong (nothing
        // ever re-adds a span, so an id lost in any cycle is still absent at the end) and costs
        // one full scan instead of one per cycle.
        assert!(
            rows >= acked_total as i64,
            "cycle {cycle}: recovered {rows} spans but {acked_total} were ACK'd across the soak \
             — an fsync-durable span was lost"
        );

        // Property 3: recovery never goes backwards.
        assert!(
            rows >= prev_rows,
            "cycle {cycle}: recovered {rows} spans, down from {prev_rows} after the previous \
             cycle — recovery destroyed data it had already recovered"
        );

        println!(
            "  cycle {cycle}/{cycles}: +{acked_this_cycle} ACK'd ({acked_total} total), \
             {} new block(s), recovered {rows} rows",
            new_blocks.len()
        );
        prev_rows = rows;
    }

    // Property 1, authoritative: every id that was handed a 200 is still there, by IDENTITY.
    // The count form above cannot see a lost ACK'd row that a surplus in-flight row replaced,
    // and every run produces that surplus, so the masking window is real rather than theoretical.
    let acked_total = acked.load(Ordering::Relaxed);
    let id_limit = seq.load(Ordering::Relaxed) as usize + 1; // no id exceeds the allocator
    let mut present = vec![false; acked_ids.len()];
    for id in recovered_span_ids(bin, data.path(), id_limit, Duration::from_secs(600)) {
        let idx = id as usize;
        if idx < present.len() {
            present[idx] = true;
        }
    }
    let missing: Vec<usize> = acked_ids
        .iter()
        .enumerate()
        .filter(|(idx, acked)| **acked && !present[*idx])
        .map(|(idx, _)| idx)
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {acked_total} ACK'd spans are missing after the final recovery (ids {:?}{}) — \
         each got a 200, so each was fsync-durable. The per-cycle count check passed, which is \
         exactly the masking a count-only assertion cannot see.",
        missing.len(),
        &missing[..missing.len().min(10)],
        if missing.len() > 10 { ", …" } else { "" }
    );

    let elapsed = started.elapsed();
    println!(
        "soak PASSED: {acked_total} spans ACK'd and recovered across {cycles} kill -9 cycles \
         in {:.1}s ({:.0} spans/s sustained), {recovered_blocks} cold blocks committed under \
         load; no loss, no double-count, no regression",
        elapsed.as_secs_f64(),
        acked_total as f64 / elapsed.as_secs_f64()
    );
}
