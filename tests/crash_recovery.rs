//! Real `kill -9` crash-recovery test — across an actual process boundary.
//!
//! The store's recovery paths (torn WAL tail, orphan Parquet, hot/cold dedup) are unit-tested
//! in-process. This test proves the END-TO-END durability claim the README/PLAN make: run the
//! real `evald serve` binary, ingest spans over OTLP so each gets a durable `200` ACK (the WAL
//! fsync is the ACK boundary), wait until a compaction has committed cold Parquet blocks, ingest
//! more spans that stay in the hot tier / WAL, then **SIGKILL the process mid-flight** and prove
//! that on restart every ACK'd span comes back exactly once — no loss, no double-count.
//!
//! This is ONE crash. `tests/soak.rs` is the same claim held under sustained load across many
//! crash/recover cycles — the GA gate (PLAN.md §6).
//!
//! `std::process::Child::kill()` sends `SIGKILL` on Unix, i.e. a genuine `kill -9` — no
//! destructors, no flush, no graceful shutdown. The binary path comes from the cargo-provided
//! `CARGO_BIN_EXE_evald`, so the test always drives the freshly-built binary.

#![cfg(unix)]

mod common;

use common::{count_spans, has_cold_block, post_span, span_body, Server};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn kill9_during_ingest_and_compaction_loses_no_acked_span() {
    let bin = env!("CARGO_BIN_EXE_evald");
    let data = tempfile::tempdir().expect("tempdir");
    let logs = tempfile::tempdir().expect("tempdir for server logs");

    // A small seal threshold + 1s compaction so continuous ingest reliably triggers several
    // hot→cold compactions while we ingest.
    let mut server = Server::start(
        bin,
        data.path(),
        logs.path().join("serve.stderr.log"),
        20,
        1,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let port = server.port;

    // A background sender posts unique spans as fast as the server accepts them, counting only
    // confirmed `200`s (each = a WAL-fsync-durable ACK) into `acked`, until told to stop. This
    // stays IN FLIGHT across the crash, so the kill -9 genuinely lands mid-ingest — not after a
    // completed batch.
    let acked = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sender = {
        let (acked, stop) = (Arc::clone(&acked), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut seq = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if post_span(port, &span_body(seq)) == Some(200) {
                    acked.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Shed (429) or, after the crash, connection refused — back off briefly.
                    std::thread::sleep(Duration::from_millis(2));
                }
                seq += 1;
            }
        })
    };

    // Let ingest run until a compaction has committed a cold Parquet block AND a healthy batch is
    // durably ACK'd — so the crash lands with the WAL hot, cold Parquet present, and quite likely
    // a flush in flight (exercising the §1.3 commit protocol + orphan-block sweep on recovery).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cold = false;
    while Instant::now() < deadline {
        cold = has_cold_block(data.path());
        if cold && acked.load(Ordering::Relaxed) >= 40 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // CRASH: a real kill -9 WHILE the sender is still posting, before any graceful path runs.
    server.kill9();
    // Server is gone; stop the sender (its in-flight/next posts now fail, uncounted) and join.
    stop.store(true, Ordering::Relaxed);
    let _ = sender.join();

    let acked = acked.load(Ordering::Relaxed);
    assert!(
        cold,
        "expected at least one compaction to commit a cold Parquet block before the crash\n\
         --- evald serve stderr (tail) ---\n{}",
        server.stderr_tail()
    );
    assert!(
        acked > 0,
        "expected some spans to be ACK'd before the crash\n\
         --- evald serve stderr (tail) ---\n{}",
        server.stderr_tail()
    );

    // RECOVER: a fresh process opens the store (replays the WAL above the watermark, sweeps any
    // orphan block from the interrupted flush) and counts what survived — with a timeout so a
    // recovery deadlock fails loudly instead of hanging.
    let (n, d) = count_spans(bin, data.path(), Duration::from_secs(60));

    // No double-count: the recovered view yields each span once.
    assert_eq!(
        n, d,
        "span_id count {d} != row count {n} — a span surfaced more than once"
    );
    // No loss of an ACK'd span: every 200 was fsync-durable, so recovery must return at least
    // that many (it may return a few more — spans fsynced just before the kill whose 200 never
    // reached us).
    assert!(
        n >= acked as i64,
        "recovered {n} spans but {acked} were ACK'd — an ACK'd span was lost across kill -9"
    );
}
