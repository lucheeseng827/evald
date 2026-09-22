//! Shared rig for the tests that drive the real `evald` binary across a process boundary.
//!
//! `tests/crash_recovery.rs` (one kill -9) and `tests/soak.rs` (the sustained C13 gate) assert
//! the same durability claim at different durations, so the machinery for standing a server up,
//! posting spans to it, killing it uncleanly and reading the store back lives here once.
//!
//! Dependency-light on purpose: std + `serde_json` only. Ingest is a hand-rolled localhost HTTP
//! POST and reads go through `evald query`, so proving durability never pulls an HTTP client
//! into the tree.

// Each integration-test binary includes this module and uses a different subset of it; the
// unused remainder is not dead code, it is code the *other* binary calls.
#![allow(dead_code)]

use std::io::{Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Grab a currently-free localhost port by binding `:0`, reading the assignment, then releasing
/// it for the server to claim.
///
/// This is a TOCTOU race by construction.
pub fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

/// Tail of the server's captured stderr, for a failure message.
pub fn stderr_tail(path: &Path) -> String {
    const MAX: u64 = 16 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return "<no stderr captured>".to_string();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX {
        let _ = f.seek(std::io::SeekFrom::Start(len - MAX));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    if buf.is_empty() {
        return "<server stderr was empty>".to_string();
    }
    // Lossy: a tail seek can land mid-UTF-8, and a diagnostic must never panic.
    String::from_utf8_lossy(&buf).into_owned()
}

/// Block until something accepts TCP connections on `port`. Returns false after `timeout`.
pub fn wait_listening(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// One minimal OTLP-JSON span whose trace_id (32 hex) and span_id (16 hex) encode `seq`, so
/// every posted span is globally unique.
pub fn span_body(seq: u64) -> String {
    let trace_id = format!("{seq:032x}");
    let span_id = format!("{seq:016x}");
    format!(
        r#"{{"resourceSpans":[{{"scopeSpans":[{{"spans":[{{"traceId":"{trace_id}","spanId":"{span_id}","name":"s","kind":1,"startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000"}}]}}]}}]}}"#
    )
}

/// POST an OTLP-JSON body to `/v1/traces` over a one-shot (`Connection: close`) HTTP/1.1
/// connection. Returns the response status code, or `None` on a connection/IO error (treated as
/// "not ACK'd").
pub fn post_span(port: u16, body: &str) -> Option<u16> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    let req = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).ok()?;
    s.flush().ok()?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).ok()?; // Connection: close → server closes after the response.
    let head = String::from_utf8_lossy(&resp);
    head.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
}

/// Number of committed Parquet blocks under `data_dir/blocks` — i.e. compactions that ran AND
/// committed (the §1.3 hot→cold commit protocol completed). Zero means compaction never landed.
pub fn cold_block_count(data_dir: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.extension().is_some_and(|x| x == "parquet") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(&data_dir.join("blocks"), &mut n);
    n
}

/// True once at least one Parquet block exists — a compaction has run and committed.
pub fn has_cold_block(data_dir: &Path) -> bool {
    cold_block_count(data_dir) > 0
}

/// Kill the child with SIGKILL and reap it, so its OS file locks are released before we reopen
/// the store. Best-effort — a test failure path must not leave a server process behind.
pub fn kill9(child: &mut Child) {
    let _ = child.kill(); // SIGKILL on Unix
    let _ = child.wait(); // reap → process fully gone, locks released
}

/// A running `evald serve` child, its port, and where its stderr went.
pub struct Server {
    pub child: Child,
    pub port: u16,
    pub stderr_log: PathBuf,
}

impl Server {
    /// Spawn `evald serve` against `data_dir` and block until it accepts connections.
    ///
    /// `seal_threshold` and `compact_interval_secs` are the two knobs that make compaction
    /// actually run under a test's ingest rate rather than never firing within its lifetime.
    ///
    /// Returns `Err` with the server's stderr tail if it never came up, having already reaped
    /// the child — a caller that cannot start a server must not also leak one.
    pub fn start(
        bin: &str,
        data_dir: &Path,
        stderr_log: PathBuf,
        seal_threshold: u64,
        compact_interval_secs: u64,
    ) -> Result<Server, String> {
        let port = free_port();
        let stderr_file = std::fs::File::create(&stderr_log).expect("create server stderr log");

        let mut cmd = Command::new(bin);
        cmd.args([
            "serve",
            "--otlp-http",
            &format!("127.0.0.1:{port}"),
            // The gRPC listener is irrelevant to what these tests assert (WAL-fsync durability
            // of HTTP-ACK'd spans across a kill -9), and `serve` joins both listeners with
            // `try_join!` — so a gRPC bind failure aborts the process before HTTP ever serves.
            // Left at its clap default this would bind the FIXED port 4317 and fail, opaquely
            // and 30s late, against any leftover evald, concurrent run, or CI runner already
            // using it. Empty disables the listener outright (src/main.rs: `"" => None`), which
            // is steadier than a second ephemeral port: it removes the collision instead of
            // doubling the `free_port` race.
            "--otlp-grpc",
            "",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--seal-threshold",
            &seal_threshold.to_string(),
            "--compact-interval-secs",
            &compact_interval_secs.to_string(),
        ])
        .stdout(Stdio::null())
        // To a FILE, not a pipe: nothing reads this handle until a test fails, and a long-lived
        // chatty child would deadlock on a full pipe buffer. `Stdio::null()` here is what made
        // the original failure a silent timeout with no diagnostic at all.
        .stderr(Stdio::from(stderr_file));

        // `serve` reads nine `EVALD_*` variables (src/main.rs). The flags above pin the five
        // that matter, but `EVALD_AUTH_TOKEN{,_FILE}`, `EVALD_MAX_HOT_SPANS` and
        // `EVALD_BLOB_OFFLOAD_BYTES` are unpinned, and any of them exported in a developer's
        // shell or a CI runner would silently reconfigure the server under the test — an auth
        // token turns every ingest POST into a 401, a small max-hot-spans sheds every span.
        // Strip the whole namespace rather than an explicit list, which goes stale the next
        // time a flag gains an `env =`.
        for (key, _) in std::env::vars() {
            if key.starts_with("EVALD_") {
                cmd.env_remove(key);
            }
        }

        let mut child = cmd.spawn().expect("spawn evald serve");
        if !wait_listening(port, Duration::from_secs(30)) {
            kill9(&mut child);
            return Err(format!(
                "server never started listening on 127.0.0.1:{port}\n\
                 --- evald serve stderr (tail) ---\n{}",
                stderr_tail(&stderr_log)
            ));
        }
        Ok(Server {
            child,
            port,
            stderr_log,
        })
    }

    /// SIGKILL the server and reap it. No graceful path runs: this is the crash under test.
    pub fn kill9(&mut self) {
        kill9(&mut self.child);
    }

    pub fn stderr_tail(&self) -> String {
        stderr_tail(&self.stderr_log)
    }
}

/// Run `evald query` to read the store, but with a hard `timeout` so a recovery DEADLOCK fails
/// the test instead of hanging the suite forever. stdout + stderr are captured (not discarded)
/// so a failure shows the real error.
///
/// Both pipes are drained by their own threads **while** the child runs, rather than after it
/// exits. Waiting first and reading afterwards deadlocks the moment the child writes more than
/// the ~64 KiB pipe buffer, and the symptom is indistinguishable from the recovery hang this
/// timeout exists to catch — the child blocks in `write`, the test blocks in `try_wait`, and
/// the failure message blames the store. A one-row aggregate makes that look impossible right
/// up until something makes the process chatty on stderr (a store with thousands of files to
/// sweep at open, say), so the harness simply does not rely on the output being small.
pub fn run_query_with_timeout(
    bin: &str,
    data_dir: &Path,
    sql: &str,
    limit: usize,
    timeout: Duration,
) -> std::process::Output {
    let mut child = Command::new(bin)
        .args([
            "query",
            sql,
            "--data-dir",
            data_dir.to_str().unwrap(),
            // `evald query` defaults to --limit 1000. Aggregates do not care, but a row-returning
            // query silently comes back truncated, which reads as catastrophic data loss to any
            // caller checking for specific rows.
            "--limit",
            &limit.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn evald query");
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try_wait on evald query") {
            Some(status) => {
                return std::process::Output {
                    status,
                    stdout: out_reader.join().unwrap_or_default(),
                    stderr: err_reader.join().unwrap_or_default(),
                }
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // Killing the child closes both pipes, so the reader threads finish and the
                // output it managed to produce is part of the failure message rather than
                // lost — a store that is merely slow usually says so on stderr first.
                let stderr = err_reader.join().unwrap_or_default();
                panic!(
                    "evald query did not finish within {timeout:?} — recovery may have \
                     deadlocked\n--- evald query stderr (tail) ---\n{}",
                    String::from_utf8_lossy(&stderr)
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Every `span_id` in the store, decoded back to the `u64` [`span_body`] encoded into it.
///
/// Runs the query with stdout redirected to a FILE, not a pipe. [`run_query_with_timeout`]
/// polls `try_wait` and only drains its pipes once the child exits, which is safe for a
/// one-row aggregate and deadlocks the moment the output exceeds the ~64 KiB pipe buffer —
/// a soak returns millions of rows. The child then blocks writing, nothing reads, and the
/// symptom is an indistinguishable "recovery may have deadlocked" timeout.
///
/// Ids are scanned out of the raw JSON rather than parsed into a `serde_json::Value` tree:
/// one `Value` per row is a large allocation when the only selected field is a 16-hex id.
/// Splitting on the key then taking the next quoted 16 characters handles compact or pretty
/// output alike.
pub fn recovered_span_ids(bin: &str, data_dir: &Path, limit: usize, timeout: Duration) -> Vec<u64> {
    let out_path = data_dir.join("..").join("span-ids.json");
    let out_file = std::fs::File::create(&out_path).expect("create span-id query output");
    let mut child = Command::new(bin)
        .args([
            "query",
            "SELECT span_id FROM spans",
            "--data-dir",
            data_dir.to_str().unwrap(),
            // `evald query` defaults to --limit 1000; without this the readback comes back
            // silently truncated and reads as catastrophic loss.
            "--limit",
            &limit.to_string(),
        ])
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn evald query for span ids");

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try_wait on evald query") {
            Some(status) => {
                assert!(
                    status.success(),
                    "evald query for span ids failed: {status}"
                );
                break;
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("evald query for span ids did not finish within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    let raw = std::fs::read_to_string(&out_path).expect("read span-id query output");
    let _ = std::fs::remove_file(&out_path);
    let mut ids = Vec::new();
    for part in raw.split("\"span_id\"").skip(1) {
        let Some(q) = part.find('"') else { continue };
        let Some(hex) = part.get(q + 1..q + 17) else {
            continue;
        };
        if let Ok(v) = u64::from_str_radix(hex, 16) {
            ids.push(v);
        }
    }
    ids
}

/// `(row_count, distinct_span_id)` for the whole store, read back through a fresh `evald query`
/// process so it exercises real recovery: WAL replay above the watermark plus the orphan-block
/// sweep from any interrupted flush.
pub fn count_spans(bin: &str, data_dir: &Path, timeout: Duration) -> (i64, i64) {
    let out = run_query_with_timeout(
        bin,
        data_dir,
        "SELECT COUNT(*) AS n, COUNT(DISTINCT span_id) AS d FROM spans",
        // One aggregate row; the limit is irrelevant here but the signature wants it.
        1,
        timeout,
    );
    assert!(
        out.status.success(),
        "evald query failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("query output not JSON: {e}\n{stdout}"));
    let row = &rows[0];
    let as_i = |v: &serde_json::Value| -> i64 {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| panic!("count not an integer: {v} (full: {stdout})"))
    };
    (as_i(&row["n"]), as_i(&row["d"]))
}
