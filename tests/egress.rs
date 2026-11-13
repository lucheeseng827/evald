//! The egress door, proved from outside (`--features judge`): a judge on 127.0.0.1 that counts
//! what it serves, and a listener on 127.0.0.2 that counts every TCP connection it accepts. With
//! `--egress-allow 127.0.0.1` nothing may reach the second; with `--offline` nothing may reach
//! either; and every request that does go out is in the ledger, by count.
#![cfg(feature = "judge")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

/// A judge on 127.0.0.1 that counts every request it receives, served or not.
async fn judge(served: Arc<AtomicUsize>) -> u16 {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|State(n): State<Arc<AtomicUsize>>| async move {
                n.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"data": [{"id": "judge-model"}]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(n): State<Arc<AtomicUsize>>| async move {
                n.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "choices": [{"message": {"content": "{\"score\": 0.9}"}}],
                    "usage": {"prompt_tokens": 30, "completion_tokens": 4}
                }))
            }),
        )
        .fallback(|State(n): State<Arc<AtomicUsize>>| async move {
            n.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::NOT_FOUND
        })
        .with_state(served);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    port
}

/// A listener on 127.0.0.2 that counts every connection it accepts, and closes it.
async fn tripwire(accepted: Arc<AtomicUsize>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((sock, _)) = l.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    });
    port
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn evald(args: Vec<String>) -> Run {
    evald_with(args, Vec::new()).await
}

/// `evald` with `env` set. No proxy variable is inherited, so a test sees only the proxies it
/// sets.
async fn evald_with(args: Vec<String>, env: Vec<(String, String)>) -> Run {
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_evald"));
        for (k, _) in std::env::vars() {
            if k.starts_with("EVALD_") || k.to_ascii_lowercase().ends_with("_proxy") {
                cmd.env_remove(k);
            }
        }
        let out = cmd.args(&args).envs(env).output().unwrap();
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

/// An eval config judging two items on `base_url`, labelled `jurisdiction`.
fn config(dir: &std::path::Path, name: &str, base_url: &str) -> String {
    std::fs::write(
        dir.join("data.jsonl"),
        "{\"id\":\"a\",\"output\":\"fine\"}\n{\"id\":\"b\",\"output\":\"also fine\"}\n",
    )
    .unwrap();
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(
        &path,
        format!(
            "dataset: ./data.jsonl\nendpoints:\n  - name: {name}\n    api: openai-chat\n    \
             base_url: {base_url}\n    retries: 0\n    jurisdiction: MY\njudges:\n  - rail: \
             toxicity\n    endpoint: {name}\n    model: judge-model\n"
        ),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn eval_run(cfg: &str, store: &str, flags: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
    a.extend(["eval", "run", "--config", cfg, "--data-dir", store].map(String::from));
    a
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_outside_the_allowlist_connects_and_every_call_is_in_the_ledger() {
    let served = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let ok_port = judge(served.clone()).await;
    let trap_port = tripwire(accepted.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store").to_string_lossy().into_owned();
    let allowed = config(
        dir.path(),
        "allowed",
        &format!("http://127.0.0.1:{ok_port}"),
    );
    let outside = config(
        dir.path(),
        "outside",
        &format!("http://127.0.0.2:{trap_port}"),
    );

    // The tripwire sees a connection when nothing stops it: the check can fail.
    let r = evald(eval_run(&outside, &store, &[])).await;
    assert_ne!(r.code, Some(0));
    assert!(
        accepted.load(Ordering::SeqCst) > 0,
        "the tripwire counts connections"
    );
    let before = accepted.load(Ordering::SeqCst);

    // With the allowlist, the outside endpoint is refused before anything connects...
    let r = evald(eval_run(
        &outside,
        &store,
        &["--egress-allow", "127.0.0.1/32"],
    ))
    .await;
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(r.stderr.contains("--egress-allow"), "{}", r.stderr);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        before,
        "nothing outside the list connected"
    );

    // ...and the allowed one is called, every request recorded.
    let r = evald(eval_run(&allowed, &store, &["--egress-allow", "127.0.0.1"])).await;
    assert_eq!(r.code, Some(0), "{}\n{}", r.stdout, r.stderr);
    let made = served.load(Ordering::SeqCst);
    assert_eq!(made, 3, "the model list and one call per item");
    let report = evald(
        ["egress", "report", "--since", "1d", "--data-dir", &store]
            .map(String::from)
            .to_vec(),
    )
    .await;
    assert_eq!(report.code, Some(0), "{}", report.stderr);
    let line = |host: &str| {
        report
            .stdout
            .lines()
            .find(|l| l.starts_with(host))
            .unwrap_or_else(|| panic!("no {host} line:\n{}", report.stdout))
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    // host, address, jurisdiction, purpose, calls, refused, errors, ...
    let ok = line("127.0.0.1");
    assert_eq!(
        (ok[1].as_str(), ok[2].as_str(), ok[3].as_str()),
        ("127.0.0.1", "MY", "judge")
    );
    assert_eq!(
        ok[4],
        made.to_string(),
        "every request the judge served is in the ledger"
    );
    assert_eq!(ok[5], "0");
    // The tripwire's line holds both runs aimed at it. Each run asks for the model list, then
    // stops at its first failed item: the first run's two calls connected and were dropped
    // (errors), even though that run failed; the second run's two were refused before
    // connecting.
    let trap = line("127.0.0.2");
    assert_eq!(
        (
            trap[1].as_str(),
            trap[4].as_str(),
            trap[5].as_str(),
            trap[6].as_str()
        ),
        ("-", "4", "2", "2"),
        "{trap:?}"
    );

    // --offline: a cached eval reruns with no network at all...
    let r = evald(eval_run(&allowed, &store, &["--offline"])).await;
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    assert_eq!(served.load(Ordering::SeqCst), made, "nothing connected");
    // ...and one that must call the judge is refused, naming the flag.
    let r = evald(eval_run(&allowed, &store, &["--offline", "--no-cache"])).await;
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(r.stderr.contains("--offline"), "{}", r.stderr);
    assert_eq!(served.load(Ordering::SeqCst), made, "nothing connected");

    // Both at once is a contradiction, refused before anything runs.
    let r = evald(eval_run(
        &allowed,
        &store,
        &["--offline", "--egress-allow", "127.0.0.1"],
    ))
    .await;
    assert_eq!(r.code, Some(2));
    assert!(r.stderr.contains("one or the other"), "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowlist_is_not_routed_around_by_a_proxy_variable() {
    let served = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let ok_port = judge(served.clone()).await;
    let proxy_port = tripwire(accepted.clone()).await;
    let proxy = format!("http://127.0.0.2:{proxy_port}");
    let env: Vec<(String, String)> = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .map(|k| (k.to_string(), proxy.clone()))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store").to_string_lossy().into_owned();
    let cfg = config(
        dir.path(),
        "allowed",
        &format!("http://127.0.0.1:{ok_port}"),
    );

    // With no allowlist the proxy variables are honoured: the calls go to the proxy, which
    // drops them, and none reaches the judge. So the check below can fail.
    let r = evald_with(eval_run(&cfg, &store, &[]), env.clone()).await;
    assert_ne!(r.code, Some(0), "{}", r.stderr);
    assert!(accepted.load(Ordering::SeqCst) > 0, "the proxy was used");
    assert_eq!(served.load(Ordering::SeqCst), 0);
    let before = accepted.load(Ordering::SeqCst);

    // With the allowlist the judge is called at the address the door admitted. The proxy is a
    // host the list does not name, so nothing goes to it.
    let r = evald_with(
        eval_run(&cfg, &store, &["--egress-allow", "127.0.0.1"]),
        env,
    )
    .await;
    assert_eq!(r.code, Some(0), "{}\n{}", r.stdout, r.stderr);
    assert_eq!(
        served.load(Ordering::SeqCst),
        3,
        "the model list and one call per item"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        before,
        "nothing went to the proxy"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn judge_probe_goes_through_the_door_too() {
    let accepted = Arc::new(AtomicUsize::new(0));
    let trap_port = tripwire(accepted.clone()).await;
    let base = format!("http://127.0.0.2:{trap_port}");
    let r = evald(
        [
            "--offline",
            "judge",
            "probe",
            "--base-url",
            &base,
            "--model",
            "m",
        ]
        .map(String::from)
        .to_vec(),
    )
    .await;
    assert_eq!(r.code, Some(1), "{}", r.stdout);
    assert!(r.stdout.contains("--offline"), "{}", r.stdout);
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
}
