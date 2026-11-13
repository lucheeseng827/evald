//! OTLP/HTTP conformance (SIGNALS.md C2): what the specification requires of a server, each as a
//! test against evald's real router (https://opentelemetry.io/docs/specs/otlp/#otlphttp).
//!
//! - A request is protobuf (`application/x-protobuf`) or its JSON mapping (`application/json`),
//!   optionally gzipped; the response uses **the same Content-Type as the request**.
//! - Full success is `200` with an `Export*ServiceResponse` (no `partial_success`).
//! - Every `4xx`/`5xx` response's body is a `google.rpc.Status`, in the request's encoding; a
//!   retryable refusal (`429`, `503`) says when to retry with `Retry-After`.
//!
//! A server that answers a protobuf request in JSON, or an error in plain text, breaks the
//! exporters that decode the reply strictly.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;
use tower::ServiceExt;

use evald::ingest::RpcStatus;
use evald::{Store, StoreConfig};

const PB: &str = "application/x-protobuf";
const JSON: &str = "application/json";

fn store(dir: &std::path::Path) -> Store {
    Store::open(
        dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .unwrap()
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(v.into())),
        }),
        ..Default::default()
    }
}

fn traces() -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "conformance")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    trace_id: vec![1; 16],
                    span_id: vec![2; 8],
                    name: "chat".into(),
                    start_time_unix_nano: 1_700_000_000_000_000_000,
                    end_time_unix_nano: 1_700_000_000_500_000_000,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn logs() -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "conformance")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: 1_700_000_000_000_000_000,
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("hello".into())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

const TRACES_JSON: &str = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":"0123456789abcdef0123456789abcdef","spanId":"0123456789abcdef","name":"chat","startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000500000000"}]}]}]}"#;

fn post(path: &str, content_type: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

struct Reply {
    status: StatusCode,
    content_type: String,
    retry_after: Option<String>,
    body: Vec<u8>,
}

async fn send(app: axum::Router, req: Request<Body>) -> Reply {
    let resp = app.oneshot(req).await.unwrap();
    let header_of = |k| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    let (status, content_type, retry_after) = (
        resp.status(),
        header_of(header::CONTENT_TYPE).unwrap_or_default(),
        header_of(header::RETRY_AFTER),
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        content_type,
        retry_after,
        body,
    }
}

/// The reply is a `google.rpc.Status` in the request's encoding; its message, decoded.
fn status_message(r: &Reply, request_json: bool) -> String {
    if request_json {
        assert_eq!(r.content_type, JSON, "a JSON request's error is JSON");
        let v: serde_json::Value = serde_json::from_slice(&r.body)
            .unwrap_or_else(|e| panic!("not a JSON Status ({e}): {:?}", r.body));
        assert!(v["code"].is_i64(), "{v}");
        v["message"].as_str().unwrap().to_string()
    } else {
        assert_eq!(r.content_type, PB, "a protobuf request's error is protobuf");
        let s = RpcStatus::decode(r.body.as_slice()).expect("a protobuf google.rpc.Status");
        assert_ne!(s.code, 0, "an error is not OK");
        s.message
    }
}

#[tokio::test]
async fn success_replies_in_the_request_s_own_format() {
    let dir = tempfile::tempdir().unwrap();
    let app = evald::ingest::router(store(dir.path()));

    let r = send(
        app.clone(),
        post("/v1/traces", PB, traces().encode_to_vec()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.content_type, PB);
    let reply = ExportTraceServiceResponse::decode(r.body.as_slice()).unwrap();
    assert!(
        reply.partial_success.is_none(),
        "full success has no partial_success"
    );

    for ct in [JSON, "application/json; charset=utf-8"] {
        let r = send(app.clone(), post("/v1/traces", ct, TRACES_JSON)).await;
        assert_eq!(r.status, StatusCode::OK, "{ct}");
        assert_eq!(r.content_type, JSON, "{ct}");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v.is_object() && v.get("partialSuccess").is_none(), "{v}");
    }

    let r = send(app.clone(), post("/v1/logs", PB, logs().encode_to_vec())).await;
    assert_eq!((r.status, r.content_type.as_str()), (StatusCode::OK, PB));
    ExportLogsServiceResponse::decode(r.body.as_slice()).unwrap();
}

#[tokio::test]
async fn a_gzipped_request_is_accepted() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let app = evald::ingest::router(store(dir.path()));
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&traces().encode_to_vec()).unwrap();
    let req = Request::post("/v1/traces")
        .header(header::CONTENT_TYPE, PB)
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(gz.finish().unwrap()))
        .unwrap();
    let r = send(app.clone(), req).await;
    assert_eq!((r.status, r.content_type.as_str()), (StatusCode::OK, PB));

    // A body that claims gzip and is not: a 400, still a Status.
    let req = Request::post("/v1/traces")
        .header(header::CONTENT_TYPE, PB)
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(&b"not gzip at all"[..]))
        .unwrap();
    let r = send(app, req).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    status_message(&r, false);
}

#[tokio::test]
async fn a_malformed_export_is_a_400_with_a_status_in_the_request_s_format() {
    let dir = tempfile::tempdir().unwrap();
    let app = evald::ingest::router(store(dir.path()));
    for path in ["/v1/traces", "/v1/logs"] {
        let r = send(
            app.clone(),
            post(path, PB, &b"\xff\xff\xff not protobuf"[..]),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{path}");
        let m = status_message(&r, false);
        assert!(m.contains("protobuf"), "{path}: {m}");

        let r = send(app.clone(), post(path, JSON, "{not json")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{path}");
        assert!(!status_message(&r, true).is_empty(), "{path}");
    }
}

#[tokio::test]
async fn a_refusal_to_store_is_retryable_with_retry_after_and_a_status() {
    let dir = tempfile::tempdir().unwrap();
    // A disk floor no disk meets: every export is refused until space is freed.
    let full = Store::open(
        dir.path(),
        StoreConfig {
            disk_min_free_bytes: u64::MAX,
            disk_check_interval: Some(std::time::Duration::from_millis(20)),
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !full.ingest_stats().disk_blocked {
        assert!(
            std::time::Instant::now() < deadline,
            "the floor was never sampled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let app = evald::ingest::router(full);
    for (ct, body, json) in [
        (PB, Body::from(traces().encode_to_vec()), false),
        (JSON, Body::from(TRACES_JSON), true),
    ] {
        let r = send(app.clone(), post("/v1/traces", ct, body)).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{ct}");
        assert!(
            r.retry_after
                .as_deref()
                .is_some_and(|s| s.parse::<u64>().is_ok()),
            "a retryable refusal says when: {:?}",
            r.retry_after
        );
        assert!(status_message(&r, json).contains("disk"), "{ct}");
    }
}

#[tokio::test]
async fn an_unauthenticated_export_is_a_401_status() {
    let dir = tempfile::tempdir().unwrap();
    let auth = evald::auth::Auth::from_tokens(["conformance-token"]).unwrap();
    let app = evald::ingest::router_with_auth(store(dir.path()), auth);
    let r = send(
        app.clone(),
        post("/v1/traces", PB, traces().encode_to_vec()),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let s = RpcStatus::decode(r.body.as_slice()).unwrap();
    assert_eq!(s.code, 16, "UNAUTHENTICATED");
    // With the token, the same export lands.
    let req = Request::post("/v1/traces")
        .header(header::CONTENT_TYPE, PB)
        .header(header::AUTHORIZATION, "Bearer conformance-token")
        .body(Body::from(traces().encode_to_vec()))
        .unwrap();
    assert_eq!(send(app, req).await.status, StatusCode::OK);
}

#[tokio::test]
async fn what_evald_does_not_serve_is_refused_as_otlp_too() {
    let dir = tempfile::tempdir().unwrap();
    let app = evald::ingest::router(store(dir.path()));
    // An exporter configured for every signal also posts metrics: refused, not a false 200.
    let r = send(app.clone(), post("/v1/metrics", PB, &b""[..])).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    let s = RpcStatus::decode(r.body.as_slice()).unwrap();
    assert_eq!(s.code, 12, "UNIMPLEMENTED");
    // The wrong method on an export path.
    let r = send(
        app.clone(),
        Request::get("/v1/traces")
            .header(header::CONTENT_TYPE, JSON)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    status_message(&r, true);
    // Paths that are not OTLP keep their own replies.
    let r = send(
        app,
        Request::get("/v1/no-such-thing")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(RpcStatus::decode(r.body.as_slice()).is_err() || !r.content_type.contains("protobuf"));
}
