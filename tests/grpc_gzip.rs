//! Node's OTLP/gRPC exporters gzip every message (Gemini CLI 0.65.0 does, on its default
//! protocol), and a server that does not inflate them refuses each `UNIMPLEMENTED`. Driven over
//! the wire, framed as such an exporter frames it: tonic's own client is compiled out of evald,
//! so this speaks HTTP/2 itself.

use std::io::Write as _;

use evald::{Auth, Store, StoreConfig};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message as _;

fn one_span() -> ExportTraceServiceRequest {
    let kv = |k: &str, v: &str| KeyValue {
        key: k.into(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(v.into())),
        }),
        ..Default::default()
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    trace_id: vec![0x77; 16],
                    span_id: vec![0x88; 8],
                    name: "chat".into(),
                    start_time_unix_nano: 1_000,
                    end_time_unix_nano: 2_000,
                    attributes: vec![kv("gen_ai.request.model", "gpt-4o")],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Send `body` (one gRPC message, gzip-compressed) to `Export`; the `grpc-status` it answers.
async fn export_gzipped(addr: std::net::SocketAddr, message: &[u8]) -> Option<String> {
    let tcp = loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    };
    let (mut client, conn) = h2::client::handshake(tcp).await.unwrap();
    tokio::spawn(conn);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(message).unwrap();
    let payload = gz.finish().unwrap();
    let mut frame = vec![1u8]; // the compressed flag
    frame.extend((payload.len() as u32).to_be_bytes());
    frame.extend(payload);
    let req = http::Request::post(format!(
        "http://{addr}/opentelemetry.proto.collector.trace.v1.TraceService/Export"
    ))
    .header("content-type", "application/grpc")
    .header("te", "trailers")
    .header("grpc-encoding", "gzip")
    .body(())
    .unwrap();
    let (resp, mut body) = client.send_request(req, false).unwrap();
    body.send_data(bytes::Bytes::from(frame), true).unwrap();
    let resp = resp.await.unwrap();
    // A refusal is trailers-only: the status in the headers.
    if let Some(s) = resp.headers().get("grpc-status") {
        return Some(s.to_str().unwrap().to_string());
    }
    let mut reply = resp.into_body();
    while let Some(chunk) = reply.data().await {
        let n = chunk.unwrap().len();
        let _ = reply.flow_control().release_capacity(n);
    }
    reply.trailers().await.unwrap().and_then(|t| {
        t.get("grpc-status")
            .map(|v| v.to_str().unwrap().to_string())
    })
}

#[tokio::test]
async fn a_gzip_compressed_export_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(evald::grpc::serve(
        addr,
        store.clone(),
        Auth::disabled(),
        async {
            let _ = stopped.await;
        },
    ));
    let status = export_gzipped(addr, &one_span().encode_to_vec()).await;
    assert_eq!(
        status.as_deref(),
        Some("0"),
        "grpc-status OK, not UNIMPLEMENTED (12)"
    );
    assert_eq!(store.span_count().unwrap(), 1);
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
