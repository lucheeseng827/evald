//! The CLI's process-level contract, which CI and orchestrators depend on: exit codes and
//! signals. Driven through the real binary, because that is where the contract lives.
//!
//! * `0` success, `1` a gate failed, `2` evald could not run (bad config, a missing file).
//! * `SIGTERM` (what `docker stop`, Kubernetes and systemd send) drains and exits `0`, as
//!   Ctrl-C does.

mod common;

use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

use common::Server;

fn evald() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_evald"));
    // A developer's or runner's EVALD_* variables must not reconfigure what is under test.
    for (key, _) in std::env::vars() {
        if key.starts_with("EVALD_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

/// A one-item eval whose `exact_match` mean is 0, gated at `threshold`.
fn eval_config(dir: &std::path::Path, threshold: f64) -> std::path::PathBuf {
    write(
        dir,
        "data.jsonl",
        r#"{"id":"a","input":"q","output":"no","expected_output":"yes"}"#,
    );
    write(
        dir,
        "eval.yaml",
        &format!(
            "name: contract\ndataset: ./data.jsonl\nevaluators:\n  - type: exact_match\n\
             thresholds:\n  exact_match: {threshold}\n"
        ),
    )
}

#[test]
fn a_failed_gate_exits_1_and_a_passing_one_0() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-dir");
    let failing = eval_config(dir.path(), 0.5);
    let out = evald()
        .args(["eval", "run", "--config"])
        .arg(&failing)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let passing = eval_config(dir.path(), 0.0);
    let out = evald()
        .args(["eval", "run", "--config"])
        .arg(&passing)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn evald_that_cannot_run_exits_2_not_1() {
    let dir = tempfile::tempdir().unwrap();
    // A config that does not exist: not a regression, so CI must be able to tell.
    let out = evald()
        .args(["eval", "run", "--config"])
        .arg(dir.path().join("missing.yaml"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("Error: "));

    // A malformed address, caught before anything binds.
    let out = evald()
        .args(["serve", "--otlp-http", "not-an-address", "--data-dir"])
        .arg(dir.path().join("d"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // clap's own usage errors were already 2.
    let out = evald().arg("no-such-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn sigterm_drains_and_exits_0() {
    let data = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let mut server = Server::start(
        env!("CARGO_BIN_EXE_evald"),
        data.path(),
        logs.path().join("serve.log"),
        1000,
        1,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    // A span in flight is not required for the contract; a store that has taken one is the
    // realistic case.
    assert_eq!(
        common::post_span(server.port, &common::span_body(1)),
        Some(200)
    );

    let pid = server.child.id() as libc::pid_t;
    // SAFETY: `pid` is our own child, still running (we have not reaped it).
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            server.kill9();
            panic!(
                "evald did not exit within 20s of SIGTERM\n{}",
                server.stderr_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "exit {status:?}\n{}",
        server.stderr_tail()
    );
    assert!(
        server.stderr_tail().contains("draining"),
        "{}",
        server.stderr_tail()
    );
}

/// `EVALD_X=` (set, empty) is how compose, Kubernetes and systemd templates pass an unset
/// optional setting. It must mean "unset", not "parse the empty string".
#[test]
fn an_empty_evald_variable_counts_as_unset() {
    let dir = tempfile::tempdir().unwrap();
    let out = evald()
        .current_dir(dir.path())
        .env("EVALD_DATA_DIR", "")
        .env("EVALD_RETENTION", "")
        .args(["sessions"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("evald-data").is_dir(),
        "the default data dir, not an empty path"
    );

    // Only the empty ones: a variable that is set is still read.
    let out = evald()
        .current_dir(dir.path())
        .env("EVALD_DATA_DIR", "chosen")
        .env("EVALD_RETENTION", "")
        .args(["sessions"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("chosen").is_dir(),
        "EVALD_DATA_DIR was read"
    );
}

/// `--logs` reaches the store: under `off`, a record is counted as dropped by policy, not read
/// the way the default `genai` would read it.
#[test]
fn the_logs_policy_reaches_the_store() {
    let data = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let server = Server::start_with(
        env!("CARGO_BIN_EXE_evald"),
        data.path(),
        logs.path().join("serve.log"),
        1000,
        1,
        &["--logs", "off"],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let record =
        r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"body":{"stringValue":"hello"}}]}]}]}"#;
    let (code, resp) = common::request(server.port, "POST", "/v1/logs", record).unwrap();
    assert_eq!(code, 200, "{resp}\n{}", server.stderr_tail());

    let (code, metrics) = common::request(server.port, "GET", "/metrics", "").unwrap();
    assert_eq!(code, 200, "{metrics}");
    assert!(
        metrics.contains(r#"evald_log_records_total{outcome="off"} 1"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"evald_log_records_total{outcome="unmapped"} 0"#),
        "{metrics}"
    );
}

/// `evald doctor` exits 1 when a check fails and 0 when none does, and it counts a token from
/// either of the sources `serve` reads. The failing check here is a non-loopback bind with no
/// auth.
#[test]
fn doctor_exits_1_on_a_problem_and_reads_the_token_where_serve_does() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write(dir.path(), "token", "0123456789abcdef\n");
    let http = format!("0.0.0.0:{}", common::free_port());
    let doctor = |env: &[(&str, &std::ffi::OsStr)]| {
        let mut cmd = evald();
        cmd.args([
            "doctor",
            "--otlp-http",
            &http,
            "--otlp-grpc",
            "",
            "--data-dir",
        ])
        .arg(dir.path().join("d"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    let (code, out) = doctor(&[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("NO authentication"), "{out}");
    let (code, out) = doctor(&[("EVALD_AUTH_TOKEN", "   ".as_ref())]);
    assert_eq!(code, Some(1), "whitespace is not a token\n{out}");

    let (code, out) = doctor(&[("EVALD_AUTH_TOKEN", "0123456789abcdef".as_ref())]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("behind the bearer-token gate"), "{out}");
    let (code, out) = doctor(&[("EVALD_AUTH_TOKEN_FILE", token_file.as_os_str())]);
    assert_eq!(code, Some(0), "{out}");
}
