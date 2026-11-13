//! Judge endpoints over real HTTP (`--features judge`): a fake OpenAI-compatible server and a
//! fake Anthropic one on loopback, and the requests the judge actually sends them.
#![cfg(feature = "judge")]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use evald::endpoint::{Api, Endpoint, StructuredOutput};
use evald::eval::DatasetItem;
use evald::judge::{JudgeSpec, Provider, Rail};

/// What the fake server saw: one line per request, `METHOD path | header=value ...`.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

impl Seen {
    fn log(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn describe(method: &str, path: &str, headers: &HeaderMap) -> String {
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(|v| format!(" {k}={v}"))
            .unwrap_or_default()
    };
    format!(
        "{method} {path} |{}{}{}{}",
        h("authorization"),
        h("x-api-key"),
        h("anthropic-version"),
        h("x-team")
    )
}

async fn serve(seen: Seen) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|State(seen): State<Seen>, headers: HeaderMap| async move {
                seen.log(describe("GET", "/v1/models", &headers));
                Json(serde_json::json!({"data": [
                    {"id": "judge-model", "root": "/models/judge-v1", "object": "model"}
                ]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    seen.log(describe("POST", "/v1/chat/completions", &headers));
                    Json(serde_json::json!({
                        "model": body["model"],
                        "choices": [{"message": {"content": "{\"score\": 0.8, \"reasoning\": \"fine\"}"}}],
                        "usage": {"prompt_tokens": 40, "completion_tokens": 6}
                    }))
                },
            ),
        )
        .route(
            "/v1/messages",
            post(
                |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    seen.log(describe("POST", "/v1/messages", &headers));
                    Json(serde_json::json!({
                        "model": body["model"],
                        "content": [{"type": "text", "text": "{\"score\": 0.4}"}],
                        "usage": {"input_tokens": 30, "output_tokens": 5}
                    }))
                },
            ),
        )
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn endpoint(name: &str, api: Api, base_url: String) -> Endpoint {
    Endpoint {
        name: name.into(),
        api,
        base_url,
        api_key_env: None,
        ca_file: None,
        client_cert: None,
        client_key: None,
        timeout: Duration::from_secs(10),
        max_in_flight: 2,
        headers: BTreeMap::new(),
        structured_output: StructuredOutput::Auto,
        retries: 3,
        jurisdiction: None,
    }
}

fn judge(name: &str, endpoint: &str) -> JudgeSpec {
    JudgeSpec {
        name: Some(name.into()),
        rail: Rail::Toxicity,
        provider: None::<Provider>,
        endpoint: Some(endpoint.into()),
        model: "judge-model".into(),
        criteria: None,
        pass_threshold: 0.5,
    }
}

fn items() -> Vec<DatasetItem> {
    ["fine", "also fine"]
        .iter()
        .map(|o| DatasetItem {
            id: None,
            input: None,
            output: (*o).into(),
            expected_output: None,
            span_id: None,
            trace_id: None,
            context: None,
            metadata: None,
        })
        .collect()
}

#[tokio::test]
async fn each_endpoint_gets_its_own_path_auth_and_headers() {
    let seen = Seen::default();
    let base = serve(seen.clone()).await;

    // A local server: keyless, with a team header. (A `/v1` written in the config is the YAML
    // test's business below; these are resolved endpoints.)
    let mut local = endpoint("local", Api::OpenaiChat, base.clone());
    local.headers.insert("x-team".into(), "evals".into());
    // A keyed gateway: another server (the same URL would be the same server, and its cached
    // scores would answer).
    let gateway = serve(seen.clone()).await;
    let mut keyed = endpoint("keyed", Api::OpenaiChat, gateway);
    keyed.api_key_env = Some("EVALD_TEST_JUDGE_ENDPOINTS_KEY".into());
    std::env::set_var("EVALD_TEST_JUDGE_ENDPOINTS_KEY", "sk-test");
    let anthropic = endpoint("anth", Api::AnthropicMessages, serve(seen.clone()).await);
    let endpoints = BTreeMap::from([
        ("local".to_string(), local),
        ("keyed".to_string(), keyed),
        ("anth".to_string(), anthropic),
    ]);
    let judges = vec![
        judge("tox_local", "local"),
        judge("tox_keyed", "keyed"),
        judge("tox_anth", "anth"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let mut scores = Vec::new();
    let aggs = evald::judge::run_all_judges(
        &judges,
        &endpoints,
        &items(),
        "run-1",
        1,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
    .unwrap();
    let mean = |n: &str| aggs.aggregates.iter().find(|a| a.name == n).unwrap().mean;
    assert_eq!(mean("tox_local"), 0.8);
    assert_eq!(mean("tox_keyed"), 0.8);
    assert_eq!(mean("tox_anth"), 0.4);

    let lines = seen.lines();
    let count = |want: &str| lines.iter().filter(|l| l.as_str() == want).count();
    assert_eq!(
        count("POST /v1/chat/completions | x-team=evals"),
        2,
        "{lines:#?}"
    );
    assert_eq!(
        count("POST /v1/chat/completions | authorization=Bearer sk-test"),
        2,
        "{lines:#?}"
    );
    assert_eq!(
        count("POST /v1/messages | anthropic-version=2023-06-01"),
        2,
        "a keyless Anthropic-API endpoint sends its version and no key: {lines:#?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains("/v1/v1")),
        "a base URL written with /v1 is not doubled: {lines:#?}"
    );
    let before = lines.len();

    // A rerun is served from the cache: the model list is read again, no judge call is made.
    let mut scores = Vec::new();
    evald::judge::run_all_judges(
        &judges,
        &endpoints,
        &items(),
        "run-2",
        2,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
    .unwrap();
    let after = seen.lines();
    assert!(
        after[before..]
            .iter()
            .all(|l| l.starts_with("GET /v1/models")),
        "{:#?}",
        &after[before..]
    );
}

/// The real binary, without the caller's `EVALD_*` environment.
fn evald() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_evald"));
    for (k, _) in std::env::vars() {
        if k.starts_with("EVALD_") {
            cmd.env_remove(k);
        }
    }
    cmd
}

/// `evald eval run` through the real binary (blocking: run it on a blocking thread).
fn eval_run(config: std::path::PathBuf, data_dir: std::path::PathBuf) -> std::process::Output {
    evald()
        .args(["eval", "run", "--config"])
        .arg(config)
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .unwrap()
}

/// `evald query` over a store, its rows as JSON.
async fn query(data_dir: &std::path::Path, sql: &str) -> Vec<serde_json::Value> {
    let (data_dir, sql) = (data_dir.to_path_buf(), sql.to_string());
    let out = tokio::task::spawn_blocking(move || {
        evald()
            .arg("query")
            .arg(sql)
            .arg("--data-dir")
            .arg(data_dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test]
async fn eval_run_takes_an_endpoints_block_from_the_config() {
    let seen = Seen::default();
    let base = serve(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("data.jsonl"),
        "{\"id\":\"a\",\"output\":\"fine\"}\n{\"id\":\"b\",\"output\":\"fine too\"}\n",
    )
    .unwrap();
    let cfg = dir.path().join("eval.yaml");
    std::fs::write(
        &cfg,
        format!(
            "name: local-judge\ndataset: ./data.jsonl\nendpoints:\n  - name: gpu-box\n    \
             api: openai-chat\n    base_url: {base}/v1/\njudges:\n  - rail: toxicity\n    \
             endpoint: gpu-box\n    model: judge-model\nthresholds:\n  judge_toxicity: 0.5\n"
        ),
    )
    .unwrap();
    let data_dir = dir.path().join("store");
    let out = tokio::task::spawn_blocking({
        let (cfg, data_dir) = (cfg.clone(), data_dir.clone());
        move || eval_run(cfg, data_dir)
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("judge_toxicity"), "{stdout}");
    assert_eq!(
        seen.lines()
            .iter()
            .filter(|l| l.starts_with("POST /v1/chat/completions"))
            .count(),
        2
    );

    // Each judge call is a span in the store, under evald's own service name...
    let spans = query(
        &data_dir,
        "SELECT name, model, prompt_tokens FROM spans WHERE service_name = 'evald.judge' \
         ORDER BY start_unix_nano",
    )
    .await;
    assert_eq!(spans.len(), 2, "{spans:#?}");
    assert_eq!(spans[0]["name"], "chat judge-model");
    assert_eq!(spans[0]["prompt_tokens"], 40);
    // ...and the run's manifest names the endpoint and the weights behind the model name.
    let manifest = query(
        &data_dir,
        "SELECT str_value FROM scores WHERE name = 'evald.run.manifest'",
    )
    .await;
    assert_eq!(manifest.len(), 1, "{manifest:#?}");
    let m: serde_json::Value =
        serde_json::from_str(manifest[0]["str_value"].as_str().unwrap()).unwrap();
    let j = &m["judges"][0];
    assert_eq!(j["endpoint"], "gpu-box");
    assert_eq!(j["base_url"], base.as_str());
    assert_eq!(j["server_model"], "/models/judge-v1");
    assert_eq!(j["calls"], 2);

    // A key the judge does not take is refused before any call.
    let bad = dir.path().join("bad.yaml");
    std::fs::write(
        &bad,
        format!(
            "dataset: ./data.jsonl\njudges:\n  - rail: toxicity\n    provider: openai\n    \
             model: m\n    base_url: {base}\n"
        ),
    )
    .unwrap();
    let out = tokio::task::spawn_blocking(move || eval_run(bad, data_dir))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("base_url"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A server that is particular about what it accepts, as small local servers are: it may refuse
/// a JSON schema, refuse a temperature (as reasoning models do), or throttle the first calls.
#[derive(Clone, Default)]
struct Picky {
    seen: Seen,
    refuse_schema: bool,
    refuse_temperature: bool,
    /// The one model that refuses a temperature (a reasoning model beside ordinary ones).
    reasoner: Option<&'static str>,
    /// How many calls still get a 429 before one is served.
    throttle: Arc<AtomicU32>,
}

async fn picky_chat(State(p): State<Picky>, Json(body): Json<serde_json::Value>) -> Response {
    let rf = body["response_format"]["type"].as_str().unwrap_or("none");
    p.seen.log(format!(
        "rf={rf} temperature={} seed={}",
        body.get("temperature").is_some(),
        body.get("seed").is_some()
    ));
    let refusal = |message: &str| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": {"message": message}})),
        )
            .into_response()
    };
    // Take one from the throttle budget if any is left. A compare-exchange loop rather than
    // `fetch_update`, which newer stable deprecates (renamed `try_update`): the mirror's CI
    // tracks stable with `-D warnings`, and this compiles clean on the pinned toolchain too.
    let throttled = {
        let mut n = p.throttle.load(Ordering::SeqCst);
        loop {
            if n == 0 {
                break false;
            }
            match p
                .throttle
                .compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break true,
                Err(current) => n = current,
            }
        }
    };
    if throttled {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "slow down",
        )
            .into_response();
    }
    if p.refuse_schema && rf == "json_schema" {
        return refusal("response_format type json_schema is not supported by this server");
    }
    let refuses_temperature =
        p.refuse_temperature || (p.reasoner.is_some() && body["model"].as_str() == p.reasoner);
    if refuses_temperature && body.get("temperature").is_some() {
        return refusal("Unsupported parameter: 'temperature' is not supported with this model.");
    }
    Json(serde_json::json!({
        "model": "served/judge-model",
        "choices": [{"message": {"content": "{\"score\": 0.9, \"reasoning\": \"ok\"}"}}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 7}
    }))
    .into_response()
}

async fn serve_picky(p: Picky) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "judge-model"}]})) }),
        )
        .route("/v1/chat/completions", post(picky_chat))
        .with_state(p);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// One judge over [`items`] against `p`, one call at a time so the order is the server's log.
async fn judge_against(p: Picky, retries: u32) -> anyhow::Result<evald::judge::JudgeOutput> {
    let mut ep = endpoint("picky", Api::OpenaiChat, serve_picky(p).await);
    ep.max_in_flight = 1;
    ep.retries = retries;
    let endpoints = BTreeMap::from([("picky".to_string(), ep)]);
    let dir = tempfile::tempdir().unwrap();
    let mut scores = Vec::new();
    evald::judge::run_all_judges(
        &[judge("tox", "picky")],
        &endpoints,
        &items(),
        "run-1",
        1,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
}

#[tokio::test]
async fn a_refused_schema_steps_down_once_for_the_run() {
    let p = Picky {
        refuse_schema: true,
        ..Picky::default()
    };
    let out = judge_against(p.clone(), 3).await.unwrap();
    assert_eq!(
        p.seen.lines(),
        [
            "rf=json_schema temperature=true seed=true",
            "rf=json_object temperature=true seed=true",
            // The second item does not ask again.
            "rf=json_object temperature=true seed=true",
        ]
    );
    let calls = &out.judges[0].calls;
    assert_eq!(calls.iter().map(|c| c.attempts).collect::<Vec<_>>(), [2, 1]);
    assert!(calls
        .iter()
        .all(|c| c.structured == StructuredOutput::JsonObject && c.parsed));
    assert_eq!(calls[0].model.as_deref(), Some("served/judge-model"));
    assert_eq!(out.aggregates[0].mean, 0.9);
    let m = out.judges[0].manifest_entry();
    assert_eq!(m["structured_output"], "json_object", "{m:#}");
}

#[tokio::test]
async fn a_refused_temperature_is_dropped_and_the_rest_is_kept() {
    let p = Picky {
        refuse_temperature: true,
        ..Picky::default()
    };
    judge_against(p.clone(), 3).await.unwrap();
    assert_eq!(
        p.seen.lines(),
        [
            "rf=json_schema temperature=true seed=true",
            "rf=json_schema temperature=false seed=true",
            "rf=json_schema temperature=false seed=true",
        ]
    );
}

#[tokio::test]
async fn a_refusal_is_remembered_for_its_model_not_the_whole_endpoint() {
    let p = Picky {
        reasoner: Some("reasoner"),
        ..Picky::default()
    };
    let mut ep = endpoint("picky", Api::OpenaiChat, serve_picky(p.clone()).await);
    ep.max_in_flight = 1;
    let endpoints = BTreeMap::from([("picky".to_string(), ep)]);
    let reasoner = JudgeSpec {
        model: "reasoner".into(),
        ..judge("tox_reasoner", "picky")
    };
    let dir = tempfile::tempdir().unwrap();
    let mut scores = Vec::new();
    evald::judge::run_all_judges(
        &[reasoner, judge("tox_plain", "picky")],
        &endpoints,
        &items(),
        "run-1",
        1,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        p.seen.lines(),
        [
            "rf=json_schema temperature=true seed=true",
            "rf=json_schema temperature=false seed=true",
            "rf=json_schema temperature=false seed=true",
            // The other model on the same endpoint still gets temperature 0.
            "rf=json_schema temperature=true seed=true",
            "rf=json_schema temperature=true seed=true",
        ]
    );
}

#[tokio::test]
async fn a_throttled_call_waits_and_retries_up_to_the_endpoint_limit() {
    let p = Picky {
        throttle: Arc::new(AtomicU32::new(2)),
        ..Picky::default()
    };
    let out = judge_against(p.clone(), 3).await.unwrap();
    assert_eq!(out.judges[0].calls[0].attempts, 3, "two 429s, then served");
    assert_eq!(out.judges[0].calls[1].attempts, 1);
    assert_eq!(p.seen.lines().len(), 4);

    // With no retries left the run fails, and says why.
    let p = Picky {
        throttle: Arc::new(AtomicU32::new(1)),
        ..Picky::default()
    };
    let err = judge_against(p, 0).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("429") && msg.contains("1 attempt"), "{msg}");
}

#[tokio::test]
async fn a_failed_call_ends_the_run_without_calling_for_the_rest() {
    let p = Picky {
        throttle: Arc::new(AtomicU32::new(u32::MAX)),
        ..Picky::default()
    };
    let err = judge_against(p.clone(), 0).await.unwrap_err();
    assert!(format!("{err:#}").contains("429"), "{err:#}");
    assert_eq!(
        p.seen.lines().len(),
        1,
        "the second item is not sent to an endpoint that already failed the run"
    );
}

/// A judge that answers by item: `item-slow-*` after a pause, `item-fails` with a 401 at once,
/// anything else at once. It logs the item of every chat request it receives.
async fn serve_mixed(seen: Seen) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "judge-model"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(seen): State<Seen>, Json(body): Json<serde_json::Value>| async move {
                    let text = body.to_string();
                    let item = [
                        "item-slow-1",
                        "item-slow-2",
                        "item-fails",
                        "item-late-1",
                        "item-late-2",
                    ]
                    .into_iter()
                    .find(|k| text.contains(k))
                    .unwrap_or("?");
                    seen.log(item.to_string());
                    if item.starts_with("item-slow") {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    if item == "item-fails" {
                        return (StatusCode::UNAUTHORIZED, "no").into_response();
                    }
                    Json(serde_json::json!({
                        "choices": [{"message": {"content": "{\"score\": 0.9}"}}],
                        "usage": {"prompt_tokens": 30, "completion_tokens": 4}
                    }))
                    .into_response()
                },
            ),
        )
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_failure_with_calls_in_flight_starts_no_more_and_records_those() {
    let seen = Seen::default();
    let mut ep = endpoint("mixed", Api::OpenaiChat, serve_mixed(seen.clone()).await);
    ep.max_in_flight = 3;
    ep.retries = 0;
    let endpoints = BTreeMap::from([("mixed".to_string(), ep)]);
    let dataset: Vec<DatasetItem> = [
        "item-slow-1",
        "item-fails",
        "item-slow-2",
        "item-late-1",
        "item-late-2",
    ]
    .iter()
    .map(|o| DatasetItem {
        id: None,
        input: None,
        output: (*o).into(),
        expected_output: None,
        span_id: None,
        trace_id: None,
        context: None,
        metadata: None,
    })
    .collect();
    let dir = tempfile::tempdir().unwrap();
    let mut scores = Vec::new();
    let err = evald::judge::run_all_judges(
        &[judge("tox", "mixed")],
        &endpoints,
        &dataset,
        "run-1",
        1,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
    .unwrap_err();
    // The first three items are in flight when the second fails. Neither later item is called,
    // although both slow calls end after the failure and free their slots.
    let mut called = seen.lines();
    called.sort();
    assert_eq!(called, ["item-fails", "item-slow-1", "item-slow-2"]);
    // The slow calls were still in flight when the run failed; their requests reached the
    // server, so they are in the failed run's egress records.
    let failed = err
        .downcast_ref::<evald::judge::FailedRun>()
        .expect("a failed judge run");
    let mut statuses: Vec<Option<u16>> = failed
        .egress
        .iter()
        .filter(|r| r.path.ends_with("/chat/completions"))
        .map(|r| r.status)
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        [Some(200), Some(200), Some(401)],
        "{:?}",
        failed.egress
    );
}

/// A server that redirects every request to `to`, as a moved (or hostile) endpoint would.
async fn serve_redirect(to: String) -> String {
    let app = Router::new().fallback(move |uri: axum::http::Uri| {
        let location = format!("{to}{uri}");
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", location)]) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_redirect_is_not_followed_with_the_key() {
    let seen = Seen::default();
    let elsewhere = serve(seen.clone()).await;
    let mut ep = endpoint(
        "moved",
        Api::AnthropicMessages,
        serve_redirect(elsewhere).await,
    );
    ep.api_key_env = Some("EVALD_TEST_JUDGE_REDIRECT_KEY".into());
    std::env::set_var("EVALD_TEST_JUDGE_REDIRECT_KEY", "sk-ant-test");
    ep.retries = 0;
    let endpoints = BTreeMap::from([("moved".to_string(), ep)]);
    let dir = tempfile::tempdir().unwrap();
    let mut scores = Vec::new();
    let err = evald::judge::run_all_judges(
        &[judge("tox", "moved")],
        &endpoints,
        &items(),
        "run-1",
        1,
        dir.path(),
        &BTreeMap::new(),
        &mut scores,
        false,
    )
    .await
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("307") && msg.contains("redirects are not followed"),
        "{msg}"
    );
    // reqwest drops `Authorization` on a cross-host hop, but not `x-api-key`.
    assert!(
        seen.lines().is_empty(),
        "a request (and its x-api-key) followed the redirect: {:#?}",
        seen.lines()
    );
}

fn probe(base_url: &str) -> std::process::Output {
    evald()
        .args([
            "judge",
            "probe",
            "--base-url",
            base_url,
            "--model",
            "judge-model",
        ])
        .output()
        .unwrap()
}

#[tokio::test]
async fn judge_probe_says_whether_an_endpoint_can_judge() {
    let p = Picky {
        refuse_schema: true,
        ..Picky::default()
    };
    let base = serve_picky(p.clone()).await;
    let url = format!("{base}/v1");
    let out = tokio::task::spawn_blocking(move || probe(&url))
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    for want in [
        "reachable",
        "scores",
        "structured output",
        "deterministic",
        "latency",
    ] {
        assert!(
            stdout.lines().any(|l| l.contains(want)),
            "no {want} line:\n{stdout}"
        );
    }
    assert!(stdout.contains("json_object"), "{stdout}");
    // The model list, then three calls: one refused schema, then json_object from there on.
    assert_eq!(p.seen.lines().len(), 4, "{:#?}", p.seen.lines());

    // Nothing listening: not usable, and the exit code says so.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let out = tokio::task::spawn_blocking(move || probe(&closed))
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("fail") && l.contains("reachable")),
        "{stdout}"
    );
}

/// A judge whose score for the item that says "wobbly" alternates call by call; every other
/// item gets the same score every time.
async fn serve_wobbly() -> String {
    let calls = Arc::new(AtomicU32::new(0));
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "judge-model"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(calls): State<Arc<AtomicU32>>, Json(body): Json<serde_json::Value>| async move {
                    let wobbly = body["messages"].to_string().contains("wobbly");
                    let score = if wobbly && calls.fetch_add(1, Ordering::SeqCst) % 2 == 1 {
                        0.3
                    } else {
                        0.9
                    };
                    Json(serde_json::json!({
                        "choices": [{"message": {"content": format!("{{\"score\": {score}}}")}}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 3}
                    }))
                },
            ),
        )
        .with_state(calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn repeat_runs_measure_the_judges_stability() {
    let base = serve_wobbly().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("data.jsonl"),
        "{\"id\":\"steady\",\"output\":\"fine\"}\n{\"id\":\"shaky\",\"output\":\"wobbly\"}\n",
    )
    .unwrap();
    let cfg = dir.path().join("eval.yaml");
    std::fs::write(
        &cfg,
        format!(
            "dataset: ./data.jsonl\nendpoints:\n  - name: box\n    api: openai-chat\n    \
             base_url: {base}\njudges:\n  - rail: toxicity\n    endpoint: box\n    \
             model: judge-model\n"
        ),
    )
    .unwrap();
    let store = dir.path().join("store");
    let run = |extra: &'static [&'static str]| {
        let (cfg, store) = (cfg.clone(), store.clone());
        tokio::task::spawn_blocking(move || {
            let out = evald()
                .args(["eval", "run", "--config"])
                .arg(&cfg)
                .arg("--data-dir")
                .arg(&store)
                .args(extra)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(out.status.success(), "{stdout}");
            stdout
                .lines()
                .find_map(|l| l.strip_prefix("run_id: "))
                .unwrap()
                .to_string()
        })
    };
    let first = run(&[]).await.unwrap();
    // A plain rerun is served from the cache, so it cannot say anything about stability...
    let cached = run(&[]).await.unwrap();
    // ...and --no-cache asks the judge again.
    let fresh = run(&["--no-cache"]).await.unwrap();

    let calibrate = |runs: String| {
        let store = store.clone();
        tokio::task::spawn_blocking(move || {
            let out = evald()
                .args(["eval", "calibrate", "--judge", "judge_toxicity", "--runs"])
                .arg(runs)
                .arg("--data-dir")
                .arg(&store)
                .output()
                .unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        })
    };
    let (code, stdout, stderr) = calibrate(format!("{first},{cached}")).await.unwrap();
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("same verdict        : 100.0%"), "{stdout}");

    let (code, stdout, stderr) = calibrate(format!("{first},{cached},{fresh}"))
        .await
        .unwrap();
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("over 3 runs"), "{stdout}");
    assert!(stdout.contains("same verdict        : 50.0%"), "{stdout}");
    assert!(stdout.contains("shaky: 0.90 0.90 0.30"), "{stdout}");
    assert!(
        !stdout.contains("steady:"),
        "a steady item is not listed: {stdout}"
    );

    // A run that never ran this judge is named.
    let (code, _, stderr) = calibrate(format!("{first},no-such-run")).await.unwrap();
    assert_eq!(code, Some(2));
    assert!(stderr.contains("no-such-run"), "{stderr}");
}

/// OTLP-JSON for `n` OpenInference model calls from service `app`, each with an output to judge.
fn model_calls(n: u64) -> String {
    let spans: Vec<String> = (1..=n)
        .map(|i| {
            format!(
                r#"{{"traceId":"{i:032x}","spanId":"{i:016x}","name":"llm","kind":3,"startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000","attributes":[{{"key":"openinference.span.kind","value":{{"stringValue":"LLM"}}}},{{"key":"llm.model_name","value":{{"stringValue":"prod-model"}}}},{{"key":"input.value","value":{{"stringValue":"hi"}}}},{{"key":"output.value","value":{{"stringValue":"hello there {i}"}}}}]}}"#
            )
        })
        .collect();
    format!(
        r#"{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"app"}}}}]}},"scopeSpans":[{{"spans":[{}]}}]}}]}}"#,
        spans.join(",")
    )
}

/// A request to the server under test, its status and body.
async fn http(port: u16, method: &'static str, path: &'static str, body: String) -> (u16, String) {
    tokio::task::spawn_blocking(move || {
        let (code, resp) = common::request(port, method, path, &body).expect("server reachable");
        let body = resp
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (code, body)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_judges_live_model_calls_in_the_background() {
    let judge = Picky::default();
    let base = serve_picky(judge.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("online.yaml");
    std::fs::write(
        &spec,
        format!(
            "endpoints:\n  - name: box\n    api: openai-chat\n    base_url: {base}\njudges:\n  \
             - rail: toxicity\n    endpoint: box\n    model: judge-model\nsample_rate: 1\n\
             daily_token_cap: 100000\n"
        ),
    )
    .unwrap();
    let (data, log) = (dir.path().join("data"), dir.path().join("serve.log"));
    let spec = spec.to_str().unwrap().to_string();
    let server = tokio::task::spawn_blocking(move || {
        common::Server::start_with(
            env!("CARGO_BIN_EXE_evald"),
            &data,
            log,
            1000,
            1,
            &["--online-eval", &spec],
        )
    })
    .await
    .unwrap()
    .unwrap_or_else(|e| panic!("{e}"));
    let port = server.port;

    // Ingest answers as it always does; the judging happens after.
    let (code, _) = http(port, "POST", "/v1/traces", model_calls(3)).await;
    assert_eq!(code, 200);
    let mut metrics = String::new();
    for _ in 0..300 {
        metrics = http(port, "GET", "/metrics", String::new()).await.1;
        if metrics.contains("evald_online_eval_judged_total{outcome=\"scored\"} 3") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        metrics.contains("evald_online_eval_judged_total{outcome=\"scored\"} 3"),
        "{metrics}\n{}",
        server.stderr_tail()
    );
    // The judge's own three calls were stored as spans, and not sampled themselves.
    assert!(
        metrics.contains("evald_online_eval_considered_total 3"),
        "{metrics}"
    );
    assert!(
        metrics.contains("evald_online_eval_tokens_total 171"),
        "{metrics}"
    );
    assert_eq!(judge.seen.lines().len(), 3, "{:#?}", judge.seen.lines());

    let (code, body) = http(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"SELECT sc.target_id, sc.num_value, s.service_name FROM scores sc JOIN spans s ON s.span_id = sc.target_id WHERE sc.config_id = 'online' ORDER BY sc.target_id"}"#.into(),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{body}");
    assert_eq!(rows[0]["target_id"], format!("{:016x}", 1));
    assert_eq!(rows[0]["num_value"], 0.9);
    assert_eq!(
        rows[0]["service_name"], "app",
        "the score is on the app's own span"
    );
    let (_, body) = http(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"SELECT count(*) AS n FROM spans WHERE service_name = 'evald.judge'"}"#.into(),
    )
    .await;
    assert!(body.contains("\"n\":3"), "{body}");
}

/// Ollama as a judge: its OpenAI-compatible model list only echoes the name, and its own
/// `/api/tags` gives the digest of the weights behind the tag. `repull` swaps the digest, as an
/// `ollama pull` of a retagged model does, and the new weights score differently.
#[derive(Clone)]
struct FakeOllama {
    digest: Arc<Mutex<String>>,
    chats: Arc<AtomicU32>,
}

impl FakeOllama {
    fn repull(&self, digest: &str) {
        *self.digest.lock().unwrap() = digest.into();
    }
}

async fn serve_ollama(o: FakeOllama) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({"object": "list", "data": [
                    {"id": "qwen2.5:7b", "object": "model", "owned_by": "library"}
                ]}))
            }),
        )
        .route(
            "/api/tags",
            get(|State(o): State<FakeOllama>| async move {
                Json(serde_json::json!({"models": [{
                    "name": "qwen2.5:7b",
                    "model": "qwen2.5:7b",
                    "digest": *o.digest.lock().unwrap(),
                    "details": {"family": "qwen2", "parameter_size": "7.6B",
                                "quantization_level": "Q4_K_M"}
                }]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(o): State<FakeOllama>| async move {
                o.chats.fetch_add(1, Ordering::SeqCst);
                let score = if o.digest.lock().unwrap().starts_with("sha256:a") {
                    0.9
                } else {
                    0.6
                };
                Json(serde_json::json!({
                    "model": "qwen2.5:7b",
                    "choices": [{"message": {"content": format!("{{\"score\": {score}}}")}}],
                    "usage": {"prompt_tokens": 30, "completion_tokens": 4}
                }))
            }),
        )
        .with_state(o);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn an_ollama_digest_is_the_judge_identity_and_a_repull_is_named() {
    let (old, new) = (
        format!("sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
    );
    let ollama = FakeOllama {
        digest: Arc::new(Mutex::new(old.clone())),
        chats: Arc::new(AtomicU32::new(0)),
    };
    let base = serve_ollama(ollama.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("data.jsonl"),
        "{\"id\":\"a\",\"output\":\"fine\"}\n{\"id\":\"b\",\"output\":\"fine too\"}\n",
    )
    .unwrap();
    let cfg = dir.path().join("eval.yaml");
    std::fs::write(
        &cfg,
        format!(
            "dataset: ./data.jsonl\nendpoints:\n  - name: ollama\n    api: openai-chat\n    \
             base_url: {base}/v1\njudges:\n  - rail: toxicity\n    endpoint: ollama\n    \
             model: qwen2.5:7b\n"
        ),
    )
    .unwrap();
    let store = dir.path().join("store");
    let run = || {
        let (cfg, store) = (cfg.clone(), store.clone());
        tokio::task::spawn_blocking(move || {
            let out = eval_run(cfg, store);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                out.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            stdout
                .lines()
                .find_map(|l| l.strip_prefix("run_id: "))
                .unwrap()
                .to_string()
        })
    };
    let first = run().await.unwrap();
    assert_eq!(ollama.chats.load(Ordering::SeqCst), 2);
    // The same weights: served from the cache.
    let _cached = run().await.unwrap();
    assert_eq!(ollama.chats.load(Ordering::SeqCst), 2, "cached");
    // The same tag over new weights is a different judge: asked again, not served old scores.
    ollama.repull(&new);
    let repulled = run().await.unwrap();
    assert_eq!(
        ollama.chats.load(Ordering::SeqCst),
        4,
        "a new digest misses the cache"
    );

    // The manifest and every judge span say which weights answered.
    let manifest = query(
        &store,
        &format!(
            "SELECT str_value FROM scores WHERE name = 'evald.run.manifest' AND id = \
             '{repulled}:manifest'"
        ),
    )
    .await;
    let m: serde_json::Value =
        serde_json::from_str(manifest[0]["str_value"].as_str().unwrap()).unwrap();
    let p = &m["judges"][0]["provenance"];
    assert_eq!(p["digest"], new.as_str());
    assert_eq!(p["quantization"], "Q4_K_M");
    assert_eq!(p["parameter_size"], "7.6B");
    assert_eq!(p["source"], "v1/models+ollama");
    assert_eq!(m["judges"][0]["server_model"], new.as_str());
    let spans = query(
        &store,
        "SELECT raw_attributes_json FROM spans WHERE service_name = 'evald.judge'",
    )
    .await;
    assert_eq!(spans.len(), 4, "{spans:#?}");
    let digests: std::collections::BTreeSet<String> = spans
        .iter()
        .map(|s| {
            let a: serde_json::Value =
                serde_json::from_str(s["raw_attributes_json"].as_str().unwrap()).unwrap();
            assert_eq!(a["evald.model.quantization"], "Q4_K_M");
            a["evald.model.digest"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(digests, [old.clone(), new.clone()].into_iter().collect());

    // `eval compare` says the judge changed, so the drop is not read as the system's alone.
    let out = tokio::task::spawn_blocking({
        let store = store.clone();
        move || {
            evald()
                .args(["eval", "compare", &first, &repulled, "--data-dir"])
                .arg(store)
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("warning: a judge changed"), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.contains("judge_toxicity") && l.contains("weights"))
        .unwrap_or_else(|| panic!("no weights line:\n{stdout}"));
    assert!(
        line.contains("sha256:aaaaaaaaaaaa… -> sha256:bbbbbbbbbbbb…"),
        "{line}"
    );

    // `judge probe` reads the same: the digest and the quantisation.
    let url = format!("{base}/v1");
    let out = tokio::task::spawn_blocking(move || {
        evald()
            .args([
                "judge",
                "probe",
                "--base-url",
                &url,
                "--model",
                "qwen2.5:7b",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("qwen2.5:7b is listed (digest bbbbbbbbbbbb, Q4_K_M, 7.6B)"),
        "{stdout}"
    );
}
