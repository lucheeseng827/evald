//! The "any attribute is queryable" guarantee (SIGNALS.md C4): every attribute a span is sent
//! with can be read back in SQL by the key it was sent with — no vendor prefix to adopt, no
//! mapping to configure, no rename — whatever its type, whether or not evald also promotes it
//! to a column, from the hot tier and from cold Parquet alike (`attr(raw_attributes_json, k)`).

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    any_value::Value, AnyValue, ArrayValue, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};

use evald::{Store, StoreConfig};

fn any(v: Value) -> Option<AnyValue> {
    Some(AnyValue { value: Some(v) })
}

fn kv(k: &str, v: Value) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: any(v),
        ..Default::default()
    }
}

/// One span carrying an attribute of every OTLP value type, under keys no mapping knows.
fn span_with_everything() -> ExportTraceServiceRequest {
    let s = |x: &str| Value::StringValue(x.into());
    let attributes = vec![
        kv("acme.tenant.id", s("t-42")),
        kv("app.retry_count", Value::IntValue(3)),
        kv("app.ratio", Value::DoubleValue(0.25)),
        kv("app.cached", Value::BoolValue(true)),
        kv(
            "app.tags",
            Value::ArrayValue(ArrayValue {
                values: vec![
                    AnyValue {
                        value: Some(s("a")),
                    },
                    AnyValue {
                        value: Some(s("b")),
                    },
                ],
            }),
        ),
        kv(
            "app.meta",
            Value::KvlistValue(KeyValueList {
                values: vec![kv("region", s("ap-southeast-1"))],
            }),
        ),
        kv("a key with spaces", s("spaced")),
        kv("ünïcødé.ключ", s("unicode")),
        // A key evald also promotes to a column: the raw attribute is still there as sent.
        kv("gen_ai.request.model", s("qwen2.5:7b")),
    ];
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", s("any-attribute"))],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    trace_id: vec![7; 16],
                    span_id: vec![9; 8],
                    name: "chat".into(),
                    start_time_unix_nano: 1_700_000_000_000_000_000,
                    end_time_unix_nano: 1_700_000_000_500_000_000,
                    attributes,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

async fn attr(store: &Store, key: &str) -> serde_json::Value {
    let sql = format!(
        "SELECT attr(raw_attributes_json, '{}') AS v FROM spans",
        key.replace('\'', "''")
    );
    let r = evald::sql::query(store, &sql, 10).await.unwrap();
    assert_eq!(r.rows.len(), 1, "{key}: {:?}", r.rows);
    r.rows[0]["v"].clone()
}

async fn every_attribute_reads_back(store: &Store, tier: &str) {
    for (key, want) in [
        ("acme.tenant.id", serde_json::json!("t-42")),
        ("app.retry_count", serde_json::json!("3")),
        ("app.ratio", serde_json::json!("0.25")),
        ("app.cached", serde_json::json!("true")),
        ("app.tags", serde_json::json!(r#"["a","b"]"#)),
        (
            "app.meta",
            serde_json::json!(r#"{"region":"ap-southeast-1"}"#),
        ),
        ("a key with spaces", serde_json::json!("spaced")),
        ("ünïcødé.ключ", serde_json::json!("unicode")),
        ("gen_ai.request.model", serde_json::json!("qwen2.5:7b")),
        ("no.such.key", serde_json::Value::Null),
    ] {
        assert_eq!(attr(store, key).await, want, "{tier}: {key}");
    }
    // Filter on one, compare a number, as a query would.
    let r = evald::sql::query(
        store,
        "SELECT count(*) AS n FROM spans WHERE attr(raw_attributes_json, 'acme.tenant.id') = 't-42' \
         AND CAST(attr(raw_attributes_json, 'app.retry_count') AS BIGINT) > 2",
        10,
    )
    .await
    .unwrap();
    assert_eq!(r.rows[0]["n"], 1, "{tier}");
}

#[tokio::test]
async fn any_attribute_is_queryable_by_its_own_key_in_both_tiers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .unwrap();
    let spans = evald::normalize::normalize_request(&span_with_everything());
    store.ingest_batch(spans, &[]).await.unwrap();

    every_attribute_reads_back(&store, "hot").await;

    // Sealed into a Parquet block, read through the cold path.
    store.seal_now().await.unwrap();
    store.compact_now().await.unwrap();
    assert_eq!(
        store.ingest_stats().hot_spans,
        0,
        "the span left the hot tier"
    );
    every_attribute_reads_back(&store, "cold").await;
}
