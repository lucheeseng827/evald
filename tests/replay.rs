//! The migration gate over real HTTP (`--features judge`): one OpenAI-compatible server on
//! loopback serving a hosted-style model A and a self-hosted model B, `evald replay` of one
//! dataset against each, and `eval compare --non-inferior` on the two runs.
#![cfg(feature = "judge")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

/// Answers "add one to N (lang)" with N + 1. Model B gets the `ms` items with N < 2 wrong.
async fn serve(served: Arc<AtomicUsize>) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|State(n): State<Arc<AtomicUsize>>| async move {
                n.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"data": [{"id": "model-a"}, {"id": "qwen2.5:7b"}]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(n): State<Arc<AtomicUsize>>, Json(body): Json<serde_json::Value>| async move {
                    n.fetch_add(1, Ordering::SeqCst);
                    let model = body["model"].as_str().unwrap_or_default().to_string();
                    let messages = body["messages"].as_array().cloned().unwrap_or_default();
                    assert_eq!(messages.len(), 1, "a replay sends the prompt alone: {body}");
                    assert!(body.get("response_format").is_none(), "{body}");
                    assert_eq!(body["temperature"], 0);
                    let prompt = messages[0]["content"].as_str().unwrap_or_default();
                    let n: i64 = prompt
                        .split_whitespace()
                        .find_map(|w| w.parse().ok())
                        .unwrap_or_default();
                    let wrong = model == "qwen2.5:7b" && prompt.contains("(ms)") && n < 2;
                    let answer = if wrong { n } else { n + 1 };
                    let prompt_tokens = if model == "model-a" { 20 } else { 25 };
                    Json(serde_json::json!({
                        "model": model,
                        "choices": [{"message": {"content": answer.to_string()}}],
                        "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 2}
                    }))
                },
            ),
        )
        .fallback(|State(n): State<Arc<AtomicUsize>>| async move {
            n.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::NOT_FOUND
        })
        .with_state(served);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn evald(args: Vec<String>) -> Out {
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_evald"));
        for (k, _) in std::env::vars() {
            if k.starts_with("EVALD_") {
                cmd.env_remove(k);
            }
        }
        let out = cmd.args(&args).output().unwrap();
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn run_id(o: &Out) -> String {
    o.stdout
        .lines()
        .find_map(|l| l.strip_prefix("run_id: "))
        .unwrap_or_else(|| panic!("no run_id:\n{}\n{}", o.stdout, o.stderr))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_two_models_and_gate_the_migration_on_non_inferiority() {
    let served = Arc::new(AtomicUsize::new(0));
    let base = serve(served.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut data = String::new();
    for i in 0..40 {
        let lang = if i < 20 { "en" } else { "ms" };
        let n = i % 20;
        data.push_str(
            &serde_json::json!({
                "id": format!("q{i}"),
                "input": format!("add one to {n} ({lang})"),
                "output": "the production answer",
                "expected_output": (n + 1).to_string(),
                "metadata": {"language": lang},
            })
            .to_string(),
        );
        data.push('\n');
    }
    std::fs::write(dir.path().join("data.jsonl"), data).unwrap();
    let cfg = dir.path().join("eval.yaml");
    std::fs::write(
        &cfg,
        format!(
            "dataset: ./data.jsonl\nevaluators:\n  - type: exact_match\nendpoints:\n  - name: \
             hosted\n    api: openai-chat\n    base_url: {base}\n    max_in_flight: 4\n    \
             jurisdiction: US\n  - name: local\n    api: openai-chat\n    base_url: {base}/v1\n    \
             max_in_flight: 4\n    jurisdiction: MY\n"
        ),
    )
    .unwrap();
    // Both models priced by the overlay (a self-hosted one at its GPU rate, in practice).
    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"model-a": {"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6},
            "qwen2.5:7b": {"input_cost_per_token": 1e-7, "output_cost_per_token": 1e-7}}"#,
    )
    .unwrap();
    let (cfg, store, prices) = (
        cfg.to_string_lossy().into_owned(),
        dir.path().join("store").to_string_lossy().into_owned(),
        prices.to_string_lossy().into_owned(),
    );
    let cand = dir.path().join("cand.jsonl").to_string_lossy().into_owned();
    let replay = |endpoint: &str, model: &str, extra: &[&str]| {
        let mut a = args(&[
            "replay",
            "--config",
            &cfg,
            "--endpoint",
            endpoint,
            "--model",
            model,
            "--data-dir",
            &store,
            "--price-table",
            &prices,
        ]);
        a.extend(args(extra));
        a
    };

    let a = evald(replay("hosted", "model-a", &[])).await;
    assert_eq!(a.code, Some(0), "{}\n{}", a.stdout, a.stderr);
    assert!(a.stdout.contains("replayed on hosted"), "{}", a.stdout);
    let b = evald(replay("local", "qwen2.5:7b", &["--out", &cand])).await;
    assert_eq!(b.code, Some(0), "{}\n{}", b.stdout, b.stderr);
    let (run_a, run_b) = (run_id(&a), run_id(&b));
    assert!(
        b.stdout
            .contains("40 call(s), 1000 input + 80 output tokens"),
        "{}",
        b.stdout
    );

    // The candidate dataset: every item, the replay's answer as its output, pairing by id.
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&cand)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 40);
    assert_eq!(lines[20]["id"], "q20");
    assert_eq!(lines[20]["output"], "0", "B got add-one-to-0 (ms) wrong");
    assert_eq!(lines[22]["output"], "3");

    // Too wide a margin to be honest about, at 0.05 the gate fails: overall B is 2/40 worse,
    // and with 40 items that cannot be shown to be within 0.05.
    let cmp = |margin: &'static str| {
        evald(args(&[
            "eval",
            "compare",
            &run_a,
            &run_b,
            "--data-dir",
            &store,
            "--non-inferior",
            "--margin",
            margin,
        ]))
    };
    let tight = cmp("0.05").await;
    assert_eq!(tight.code, Some(1), "{}\n{}", tight.stdout, tight.stderr);
    assert!(
        tight.stdout.contains("NOT SHOWN NON-INFERIOR"),
        "{}",
        tight.stdout
    );
    let wide = cmp("0.2").await;
    assert_eq!(wide.code, Some(0), "{}\n{}", wide.stdout, wide.stderr);
    let verdict = |slice: &str| {
        wide.stdout
            .lines()
            .find(|l| l.starts_with("exact_match") && l.contains(&format!(" {slice} ")))
            .unwrap_or_else(|| panic!("no {slice} row:\n{}", wide.stdout))
            .split_whitespace()
            .last()
            .unwrap()
            .to_string()
    };
    // Overall shown within 0.2; `en` unchanged; `ms` (2 of 20 worse) cannot be told at 0.2.
    assert_eq!(verdict("all"), "non-inferior");
    assert_eq!(verdict("language=en"), "non-inferior");
    assert_eq!(verdict("language=ms"), "inconclusive");
    assert!(
        wide.stdout.contains("NON-INFERIOR — every evaluator"),
        "{}",
        wide.stdout
    );
    // The cost and latency of each side, from the replays.
    for want in [
        "hosted (model-a)",
        "local (qwen2.5:7b)",
        "US",
        "MY",
        "$0.0010",
        "$0.0001",
        "-0.0009 $",
        "latency p50",
    ] {
        assert!(wide.stdout.contains(want), "no {want:?}:\n{}", wide.stdout);
    }
    // At compare time, both replays can be priced at other rates (a GPU rate known later).
    let dearer = dir.path().join("dearer.json");
    std::fs::write(
        &dearer,
        r#"{"qwen2.5:7b": {"input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6}}"#,
    )
    .unwrap();
    let repriced = evald(args(&[
        "eval",
        "compare",
        &run_a,
        &run_b,
        "--data-dir",
        &store,
        "--price-table",
        &dearer.to_string_lossy(),
    ]))
    .await;
    assert_eq!(repriced.code, Some(0), "{}", repriced.stderr);
    // 40 calls × (25 + 2) tokens × $1e-6.
    assert!(repriced.stdout.contains("$0.0011"), "{}", repriced.stdout);
    // --margin is required, never defaulted.
    let none = evald(args(&[
        "eval",
        "compare",
        &run_a,
        &run_b,
        "--data-dir",
        &store,
        "--non-inferior",
    ]))
    .await;
    assert_eq!(none.code, Some(2), "{}", none.stderr);
    assert!(none.stderr.contains("--margin"), "{}", none.stderr);

    // Each call is a model-call span, and the candidate's scores are on B's calls.
    let count = |q: String| {
        let store = store.clone();
        async move {
            let o = evald(args(&["query", &q, "--data-dir", &store])).await;
            let rows: serde_json::Value = serde_json::from_str(&o.stdout)
                .unwrap_or_else(|e| panic!("{e}: {}\n{}", o.stdout, o.stderr));
            rows[0]["n"].as_u64().unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) AS n FROM spans WHERE service_name = 'evald.replay'".into()).await,
        80
    );
    let on_b = count(format!(
        "SELECT count(*) AS n FROM scores sc JOIN spans s ON s.span_id = sc.target_id WHERE \
         s.service_name = 'evald.replay' AND sc.name = 'exact_match' AND s.raw_attributes_json \
         LIKE '%{run_b}%'"
    ))
    .await;
    assert_eq!(on_b, 40);

    // The calls went through the egress door: in the ledger as `replay`, with each endpoint's
    // jurisdiction, and every request the server saw is there.
    let report = evald(args(&[
        "egress",
        "report",
        "--since",
        "1d",
        "--data-dir",
        &store,
    ]))
    .await;
    assert_eq!(report.code, Some(0), "{}", report.stderr);
    let rows: Vec<Vec<&str>> = report
        .stdout
        .lines()
        .filter(|l| l.starts_with("127.0.0.1"))
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert!(rows.iter().all(|r| r[3] == "replay"), "{}", report.stdout);
    let calls: usize = rows.iter().map(|r| r[4].parse::<usize>().unwrap()).sum();
    assert_eq!(calls, served.load(Ordering::SeqCst), "{}", report.stdout);
    assert!(rows.iter().any(|r| r[2] == "MY") && rows.iter().any(|r| r[2] == "US"));

    // --offline: refused before anything connects.
    let before = served.load(Ordering::SeqCst);
    let mut off = vec!["--offline".to_string()];
    off.extend(replay("local", "qwen2.5:7b", &[]));
    let r = evald(off).await;
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(r.stderr.contains("--offline"), "{}", r.stderr);
    assert_eq!(served.load(Ordering::SeqCst), before);

    // A dataset that cannot pair is refused before any call.
    std::fs::write(
        dir.path().join("data.jsonl"),
        "{\"input\":\"add one to 1\",\"output\":\"x\"}\n",
    )
    .unwrap();
    let r = evald(replay("local", "qwen2.5:7b", &[])).await;
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(
        r.stderr.contains("give dataset items an `id`"),
        "{}",
        r.stderr
    );
    assert_eq!(served.load(Ordering::SeqCst), before);
}
