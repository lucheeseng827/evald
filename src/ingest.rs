//! OTLP/HTTP receiver + query/score/SQL API + embedded SPA — PoC build steps 1–8.
//!
//! The HTTP surface of evald:
//!
//! - `POST /v1/traces` — OTLP ingest. Accepts protobuf (`application/x-protobuf`,
//!   gzip-aware) and OTLP-JSON (`application/json`), decodes into the same
//!   opentelemetry-proto types (the JSON path bridges the int64-as-string gap via
//!   [`coerce_otlp_json_ints`]), runs one [`crate::normalize`] pass, then **durably
//!   appends** to the [`Store`]. The reply mirrors the request format; on overload the
//!   bounded ingest channel sheds with `429 + Retry-After` (never a silent drop).
//! - `GET /v1/spans` — read recent spans back, optionally filtered by `trace_id`.
//! - `GET /v1/traces/{trace_id}` — all spans of one trace, in arrival order.
//! - `POST /v1/scores` / `GET /v1/scores[/{id}]` — the universal [`Score`] object
//!   (eval / human / API), targeting a span/trace/session/run.
//! - `POST /v1/span_annotations` — Phoenix-compatible `{ "data": [ … ] }` envelope,
//!   mapped onto span-targeted scores (with optional `identifier` upsert).
//! - `POST /v1/sql` — read-only DataFusion SQL over the cold Parquet blocks + the score
//!   store ([`crate::sql`]); returns `{ columns, rows, row_count, truncated }`.
//! - everything else — the embedded SPA ([`crate::ui`]), served as the router fallback
//!   so it never shadows a `/v1/*` route.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path as AxPath, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;
use serde_json::Value as Json2;
use std::net::SocketAddr;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    auth::Auth, normalize, DataType, NormalizedSpan, Score, ScoreSource, ScoreTarget, Store,
    StoreConfig, StoreError, Tokens, UsageMissingReason,
};

/// Cap a single OTLP batch body. OTLP batches are bounded by the SDK's
/// `max_export_batch_size`; 16 MiB is generous headroom while still bounding memory.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default cap on `GET /v1/spans` results.
const DEFAULT_SPAN_LIMIT: usize = 100;
const MAX_SPAN_LIMIT: usize = 10_000;

/// Default / max rows returned by `POST /v1/sql` (the result is capped, not the scan).
const DEFAULT_SQL_LIMIT: usize = 1_000;
const MAX_SQL_LIMIT: usize = 100_000;

/// Upper bound on the store-wide scan behind `GET /v1/sessions/{id}`. The first-cut session
/// rollup scans the most-recent spans and filters by `session_id` (there is no session→span
/// index yet); this bounds that scan so a huge store can't make one request unbounded. When
/// the scan hits this cap the rollup is flagged `truncated` (it may omit older session spans).
const SESSION_SCAN_CAP: usize = 50_000;

const CT_PROTOBUF: &str = "application/x-protobuf";
const CT_JSON: &str = "application/json";

/// Build the receiver + query router over a [`Store`], with **no** authentication (the
/// default local posture). Separated from [`serve`] so tests can drive it via
/// `tower::ServiceExt::oneshot` without binding a socket.
pub fn router(store: Store) -> Router {
    build_router(store, MAX_BODY_BYTES, Auth::disabled())
}

/// Like [`router`], but gated by a bearer-token [`Auth`] — when the gate is armed, every
/// request (OTLP ingest, `/v1/*`, and the SPA) must present a valid `Authorization: Bearer
/// <token>` or get a `401`. `serve` uses this; a [`disabled`](Auth::disabled) gate makes it
/// identical to [`router`].
pub fn router_with_auth(store: Store, auth: Auth) -> Router {
    build_router(store, MAX_BODY_BYTES, auth)
}

/// Assemble the router with a configurable request-body limit (tests use a small one) and an
/// optional bearer-token gate.
fn build_router(store: Store, body_limit: usize, auth: Auth) -> Router {
    let router = Router::new()
        .route("/v1/traces", post(export_traces))
        .route("/v1/spans", get(get_spans))
        .route("/v1/traces/{trace_id}", get(get_trace))
        .route("/v1/sessions/{session_id}", get(get_session))
        .route("/v1/scores", post(post_scores).get(get_scores))
        .route("/v1/scores/{id}", get(get_score_by_id))
        .route("/v1/span_annotations", post(post_span_annotations))
        .route("/v1/sql", post(post_sql))
        .route("/v1/stats", get(get_stats))
        .route("/v1/meta", get(get_meta))
        .route("/v1/blobs/{key}", get(get_blob))
        // Everything not matched above is the embedded SPA (served by rust-embed, with an
        // index.html fallback for client-side routes). Kept as the fallback so it never
        // shadows a `/v1/*` API route.
        .fallback(crate::ui::static_handler)
        // Layer order matters: the last `.layer()` is the OUTERMOST (runs first). We want
        // decompression outermost so it inflates the body BEFORE `DefaultBodyLimit` (inner)
        // measures it — so the cap applies to the DECOMPRESSED size, stopping a small gzip
        // from inflating into unbounded memory. (Adding the limit first / decompression last.)
        .layer(DefaultBodyLimit::max(body_limit))
        // Transparently inflate `Content-Encoding: gzip` request bodies (OTLP exporters
        // gzip by default) before the handler — and before the body limit — sees them.
        .layer(RequestDecompressionLayer::new());
    // The bearer-token gate is the OUTERMOST layer (added last → runs first): an
    // unauthenticated request is rejected before we spend any work decompressing or decoding
    // its body, so the gate also blunts a decompression bomb from an anonymous client. The
    // layer is only added when the gate is armed, so the disabled path is byte-for-byte the
    // pre-auth router (zero added overhead, and every existing test exercises it unchanged).
    let router = if auth.is_enabled() {
        router.layer(axum::middleware::from_fn_with_state(auth, require_bearer))
    } else {
        router
    };
    router.with_state(store)
}

/// Auth middleware: when the gate is armed, require a valid `Authorization: Bearer <token>`
/// on every request, else `401` with a `WWW-Authenticate: Bearer` challenge. Only installed
/// when [`Auth::is_enabled`], so it never runs (nor allocates) in the default no-auth mode.
async fn require_bearer(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if auth.check(header) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "evald: unauthorized — set Authorization: Bearer <token>\n",
        )
            .into_response()
    }
}

/// Open the store and serve the OTLP/HTTP receiver + query API — and, when `grpc_addr` is
/// `Some`, the OTLP/gRPC receiver on that address — until Ctrl-C. Both listeners share one
/// [`Store`] (so one ingest core, one durability path), one graceful-shutdown signal, and one
/// bearer-token [`Auth`] gate (a [`disabled`](Auth::disabled) gate leaves both listeners
/// open, the default local posture).
pub async fn serve(
    addr: SocketAddr,
    grpc_addr: Option<SocketAddr>,
    data_dir: PathBuf,
    config: StoreConfig,
    auth: Auth,
) -> anyhow::Result<()> {
    let store = Store::open(&data_dir, config)
        .map_err(|e| anyhow::anyhow!("failed to open store at {}: {e}", data_dir.display()))?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    // First-run banner: the exact reachable URLs (the recurring "which URL / port do I hit?"
    // self-host pain, e.g. Opik #1607) + where data lives, so onboarding isn't a scavenger hunt.
    let abs_data = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    tracing::info!("evald ready:");
    tracing::info!("  UI       http://{local}/");
    tracing::info!(
        "  ingest   POST http://{local}/v1/traces  (OTEL_EXPORTER_OTLP_ENDPOINT=http://{local})"
    );
    if let Some(gaddr) = grpc_addr {
        tracing::info!(
            "  ingest   gRPC {gaddr}  (OTEL_EXPORTER_OTLP_ENDPOINT=http://{gaddr}, protocol=grpc)"
        );
    }
    tracing::info!(
        "  query    GET  http://{local}/v1/spans  ·  GET http://{local}/v1/traces/{{trace_id}}"
    );
    tracing::info!(
        "  data     {}  (mount as a volume to persist across restarts)",
        abs_data.display()
    );
    // Loud warning when the data dir is on ephemeral storage — spans won't survive a restart, the
    // "self-hosted, then lost my data" onboarding pain. Never fails startup; just flags it.
    if is_ephemeral_path(&abs_data) {
        tracing::warn!(
            data_dir = %abs_data.display(),
            "data dir looks EPHEMERAL — spans will NOT survive a restart; mount a persistent volume"
        );
    }
    // Auth posture — the last piece of the "is this safe to expose?" story. Logs whether the
    // bearer gate is armed, and shouts if any listener is bound off-loopback with no gate.
    log_auth_posture(local, grpc_addr, &auth);

    // One shutdown signal fans out to every listener: a Ctrl-C flips the watch, and each
    // server's graceful-shutdown future wakes on it and drains.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let on_shutdown = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.changed().await;
    };

    let http = axum::serve(listener, router_with_auth(store.clone(), auth.clone()))
        .with_graceful_shutdown(on_shutdown(shutdown_rx.clone()));

    match grpc_addr {
        Some(gaddr) => {
            let grpc = crate::grpc::serve(gaddr, store, auth, on_shutdown(shutdown_rx.clone()));
            tokio::try_join!(async { http.await.map_err(anyhow::Error::from) }, grpc)?;
        }
        None => http.await?,
    }
    Ok(())
}

/// Log the authentication posture at startup: whether the bearer gate is armed, and — the
/// important safety net — a loud warning when a listener is bound off-loopback with no gate,
/// which is exactly the "exposed evald with no auth" footgun `SECURITY.md` warns about.
fn log_auth_posture(http: SocketAddr, grpc: Option<SocketAddr>, auth: &Auth) {
    let off_loopback = |a: SocketAddr| !a.ip().is_loopback();
    let exposed = off_loopback(http) || grpc.is_some_and(off_loopback);
    if auth.is_enabled() {
        tracing::info!(
            tokens = auth.token_count(),
            "auth: bearer-token gate ARMED — every request needs Authorization: Bearer <token>"
        );
        if exposed {
            // Bearer over plaintext is only as private as the transport — remind, don't block.
            tracing::info!(
                "auth: a listener is bound off-loopback; terminate TLS at a reverse proxy \
                 (or edgeguard) if the network is untrusted — evald itself does no TLS"
            );
        }
    } else if exposed {
        tracing::warn!(
            http = %http,
            grpc = grpc.map(|g| g.to_string()).unwrap_or_default(),
            "auth: NO authentication and a listener is bound OFF-LOOPBACK — anyone who can \
             reach the port can read/write all spans, scores, and run SQL. Set --auth-token / \
             EVALD_AUTH_TOKEN (or --auth-token-file), or put an authenticating reverse proxy \
             in front. See SECURITY.md."
        );
    } else {
        tracing::info!("auth: none (loopback-only) — the default local posture");
    }
}

/// Heuristic: does `dir` live under a classic ephemeral location (a container tmpfs / scratch),
/// warranting a "your data won't survive a restart" warning? Component-based prefix match against
/// known-ephemeral roots — conservative, so it flags only the obvious cases (`/tmp`, `/var/tmp`,
/// `/dev/shm`, `/run`) and never a real data directory like `/var/lib/evald`.
fn is_ephemeral_path(dir: &std::path::Path) -> bool {
    const EPHEMERAL_ROOTS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm", "/run"];
    EPHEMERAL_ROOTS.iter().any(|root| dir.starts_with(root))
}

#[cfg(test)]
mod onboarding_tests {
    use super::is_ephemeral_path;
    use std::path::Path;

    #[test]
    fn ephemeral_paths_are_flagged_but_real_data_dirs_are_not() {
        assert!(is_ephemeral_path(Path::new("/tmp/evald-data")));
        assert!(is_ephemeral_path(Path::new("/var/tmp/x")));
        assert!(is_ephemeral_path(Path::new("/dev/shm/x")));
        assert!(is_ephemeral_path(Path::new("/run/evald")));
        assert!(!is_ephemeral_path(Path::new("/var/lib/evald")));
        assert!(!is_ephemeral_path(Path::new("/home/user/evald-data")));
        // Component-based prefix: `/tmpfoo` is NOT under `/tmp`, so it isn't flagged.
        assert!(!is_ephemeral_path(Path::new("/tmpfoo")));
    }
}

/// Resolve when Ctrl-C is received, triggering axum's graceful shutdown.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received — draining");
}

/// `POST /v1/traces` — decode (protobuf or JSON), normalize, durably store.
async fn export_traces(State(store): State<Store>, headers: HeaderMap, body: Bytes) -> Response {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"));

    let request = if is_json {
        match decode_otlp_json(&body) {
            Ok(req) => req,
            Err(err) => {
                tracing::warn!(%err, bytes = body.len(), "rejected: OTLP-JSON decode failed");
                return (
                    StatusCode::BAD_REQUEST,
                    format!("evald: could not decode OTLP-JSON: {err}\n"),
                )
                    .into_response();
            }
        }
    } else {
        match ExportTraceServiceRequest::decode(body.as_ref()) {
            Ok(req) => req,
            Err(err) => {
                tracing::warn!(%err, bytes = body.len(), "rejected: OTLP protobuf decode failed");
                return (
                    StatusCode::BAD_REQUEST,
                    format!("evald: could not decode OTLP protobuf: {err}\n"),
                )
                    .into_response();
            }
        }
    };

    let mut spans = normalize::normalize_request(&request);
    let count = spans.len();
    log_ingest(request.resource_spans.len(), &spans);

    // Offload oversized input/output payloads to the blob store BEFORE the durable append, so
    // the WAL and Parquet blocks only ever carry a compact `evald-blob:<key>` reference.
    store.offload_payloads(&mut spans);

    match store.append(spans).await {
        Ok(()) => success_response(is_json),
        Err(StoreError::Backpressure) => {
            tracing::warn!(count, "shedding: ingest channel full");
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "1")],
                "evald: overloaded, retry shortly\n",
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(%err, count, "store write failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "evald: store unavailable\n",
            )
                .into_response()
        }
    }
}

/// `GET /v1/spans?trace_id=&limit=` — recent spans, most-recent-first.
#[derive(serde::Deserialize)]
struct SpansQuery {
    trace_id: Option<String>,
    limit: Option<usize>,
}

/// A span as returned by the read APIs: the normalized span plus a computed
/// `usage_missing` diagnostic. The diagnostic is derived (not stored), so it is identical
/// for hot- and cold-tier spans and adds no Parquet column. Absent (via
/// `skip_serializing_if`) for non-LLM spans and LLM spans that DO carry usage, so the
/// wire shape is unchanged for the common case and only the flagged spans grow a key.
#[derive(serde::Serialize)]
struct SpanView {
    #[serde(flatten)]
    span: NormalizedSpan,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_missing: Option<UsageMissingReason>,
    /// Set only on the full-trace read: this span's `parent_span_id` references a span that
    /// is NOT present in the trace — broken nesting, the usual signature of a dropped or
    /// sampled-out intermediate span (the recurring "my trace tree has holes / orphaned
    /// spans" pain). Absent on `/v1/spans` (a paged read where a missing parent may simply
    /// be on another page) and absent for well-formed spans, so the wire shape is unchanged
    /// for the healthy case.
    #[serde(skip_serializing_if = "Option::is_none")]
    orphan_parent: Option<bool>,
}

impl From<NormalizedSpan> for SpanView {
    fn from(span: NormalizedSpan) -> Self {
        let usage_missing = span.usage_missing();
        SpanView {
            span,
            usage_missing,
            orphan_parent: None,
        }
    }
}

fn views(spans: Vec<NormalizedSpan>) -> Vec<SpanView> {
    spans.into_iter().map(SpanView::from).collect()
}

/// Build span views WITH full-trace context so the broken-nesting diagnostic can be
/// computed: a span whose `parent_span_id` points at a span absent from `spans` is flagged
/// `orphan_parent`. Only sound when `spans` is a COMPLETE trace (the `/v1/traces/{id}`
/// read); on a partial/paged read a dangling parent could just be elsewhere, so `views()`
/// is used there instead.
fn views_in_trace(spans: Vec<NormalizedSpan>) -> Vec<SpanView> {
    let present: HashSet<String> = spans.iter().map(|s| s.span_id.clone()).collect();
    spans
        .into_iter()
        .map(|s| {
            let orphan = s
                .parent_span_id
                .as_deref()
                .is_some_and(|p| !present.contains(p));
            let mut view = SpanView::from(s);
            if orphan {
                view.orphan_parent = Some(true);
            }
            view
        })
        .collect()
}

/// Run a blocking `Store` read (`query`/`trace`, either of which can hit `cold::read_block`,
/// direct Parquet I/O) on the blocking pool instead of inline on the async handler's Tokio
/// worker, so a cold-tier scan can't monopolize that worker. A panicked blocking task is mapped
/// to an `io::Error` so callers can still funnel it through the existing `query_error` path.
async fn query_blocking<F>(f: F) -> std::io::Result<Vec<NormalizedSpan>>
where
    F: FnOnce() -> std::io::Result<Vec<NormalizedSpan>> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => Err(std::io::Error::other(join_err)),
    }
}

/// Handle `GET /v1/spans` — recent spans (optionally filtered by `trace_id`).
async fn get_spans(State(store): State<Store>, Query(q): Query<SpansQuery>) -> Response {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_SPAN_LIMIT)
        .clamp(1, MAX_SPAN_LIMIT);
    let trace_id = q.trace_id.clone();
    match query_blocking(move || store.query(trace_id.as_deref(), limit)).await {
        Ok(spans) => Json(views(spans)).into_response(),
        Err(err) => query_error(err),
    }
}

/// `GET /v1/traces/{trace_id}` — all spans of one trace, in arrival order.
async fn get_trace(State(store): State<Store>, AxPath(trace_id): AxPath<String>) -> Response {
    match query_blocking(move || store.trace(&trace_id)).await {
        Ok(spans) if spans.is_empty() => {
            (StatusCode::NOT_FOUND, "evald: no spans for that trace_id\n").into_response()
        }
        // Full-trace context is available here, so flag broken parent nesting (orphaned
        // spans) — the one diagnostic that needs the complete span set to compute.
        Ok(spans) => Json(views_in_trace(spans)).into_response(),
        Err(err) => query_error(err),
    }
}

/// A session/thread rollup: cost + token + span/trace totals over every span carrying a
/// given `session.id`. The recurring "I need per-conversation cost, not per-span" ask
/// (session-level attribution across a multi-turn thread). Computed from a bounded scan;
/// `scanned`/`truncated` make the bound explicit rather than silently partial.
#[derive(serde::Serialize)]
struct SessionRollup {
    session_id: String,
    span_count: usize,
    /// Distinct traces the session's spans belong to (a session usually spans many traces).
    trace_count: usize,
    /// Summed `cost_usd` over spans that reported one; absent when none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    /// Field-wise token totals (only populated fields are summed / emitted).
    #[serde(skip_serializing_if = "Tokens::is_empty")]
    tokens: Tokens,
    /// Earliest span start / latest span end (unix nanos) — the session's wall-clock span.
    start_unix_nano: u64,
    end_unix_nano: u64,
    /// Total spans scanned store-wide to build this rollup (the bounded first-cut scan).
    scanned: usize,
    /// True when the scan hit `SESSION_SCAN_CAP`, so older session spans may be omitted; a
    /// dedicated session→span index is the follow-up.
    truncated: bool,
}

/// Fold one span's populated token fields into the running per-field totals.
fn add_tokens(acc: &mut Tokens, t: &Tokens) {
    fn add(acc: &mut Option<u64>, v: Option<u64>) {
        if let Some(v) = v {
            *acc = Some(acc.unwrap_or(0) + v);
        }
    }
    add(&mut acc.prompt, t.prompt);
    add(&mut acc.completion, t.completion);
    add(&mut acc.total, t.total);
    add(&mut acc.cache_read, t.cache_read);
    add(&mut acc.cache_write, t.cache_write);
    add(&mut acc.reasoning, t.reasoning);
}

/// `GET /v1/sessions/{session_id}` — cost/token/span rollup for one session (thread).
async fn get_session(State(store): State<Store>, AxPath(session_id): AxPath<String>) -> Response {
    let scanned = match query_blocking(move || store.query(None, SESSION_SCAN_CAP)).await {
        Ok(spans) => spans,
        Err(err) => return query_error(err),
    };
    let scanned_count = scanned.len();
    let truncated = scanned_count >= SESSION_SCAN_CAP;

    let mut span_count = 0usize;
    let mut traces: HashSet<String> = HashSet::new();
    let mut cost: Option<f64> = None;
    let mut tokens = Tokens::default();
    let mut start = u64::MAX;
    let mut end = 0u64;

    for s in &scanned {
        if s.session_id.as_deref() != Some(session_id.as_str()) {
            continue;
        }
        span_count += 1;
        traces.insert(s.trace_id.clone());
        if let Some(c) = s.cost_usd {
            cost = Some(cost.unwrap_or(0.0) + c);
        }
        add_tokens(&mut tokens, &s.tokens);
        start = start.min(s.start_unix_nano);
        end = end.max(s.end_unix_nano);
    }

    if span_count == 0 {
        return (
            StatusCode::NOT_FOUND,
            "evald: no spans for that session_id\n",
        )
            .into_response();
    }

    Json(SessionRollup {
        session_id,
        span_count,
        trace_count: traces.len(),
        cost_usd: cost,
        tokens,
        start_unix_nano: start,
        end_unix_nano: end,
        scanned: scanned_count,
        truncated,
    })
    .into_response()
}

/// `GET /v1/stats` — ingest-pipeline load: hot-tier backlog, channel depth, cumulative
/// sheds, and whether ingest is currently shedding. The signal an operator needs to see the
/// store approaching its shed threshold before it starts returning 429s.
async fn get_stats(State(store): State<Store>) -> Response {
    Json(store.ingest_stats()).into_response()
}

/// `GET /v1/meta` — the edition/capability handshake the embedded console reads once at
/// boot to decide which surfaces to render. The OSS node is a single local store: no fleet,
/// no tenants. `judge` reflects whether this build can make an outbound LLM-judge call (the
/// `judge` cargo feature) so the console can label BYO-key vs offline. The EE `fleet_query`
/// node serves the SAME console bytes but answers this route with `edition:"ee", fleet:true`,
/// which is how one embedded SPA lights up the Fleet · EE nav group only where it applies.
async fn get_meta() -> Response {
    Json(serde_json::json!({
        "edition": "oss",
        "fleet": false,
        "judge": cfg!(feature = "judge"),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// `GET /v1/blobs/{key}` — fetch an offloaded payload by the key in an `evald-blob:<key>`
/// reference. Returns the raw bytes (`application/octet-stream`), or `404` for an unknown /
/// malformed key. Bytes are opaque on purpose — the offload treats a payload as a blob, not
/// tokenizable text — so the caller renders/decodes as it sees fit.
async fn get_blob(State(store): State<Store>, AxPath(key): AxPath<String>) -> Response {
    match store.get_blob(&key) {
        Ok(Some(bytes)) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "evald: no such blob\n").into_response(),
        Err(err) => query_error(err),
    }
}

/// Map a store read error to a `500` response (logging the cause).
fn query_error(err: std::io::Error) -> Response {
    tracing::error!(%err, "query failed reading the store");
    (StatusCode::INTERNAL_SERVER_ERROR, "evald: query failed\n").into_response()
}

// --- scores (PoC step 5) ---------------------------------------------------------

/// Body of `POST /v1/scores` — a single score or an array. evald-native shape.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ScoresBody {
    One(Box<ScoreInput>),
    Many(Vec<ScoreInput>),
}

/// One evald-native score as posted. Exactly one target id (span preferred) is required,
/// plus a value (`value` as number/string/bool, or explicit `num_value`/`str_value`).
#[derive(serde::Deserialize)]
struct ScoreInput {
    id: Option<String>,
    trace_id: Option<String>,
    span_id: Option<String>,
    session_id: Option<String>,
    run_id: Option<String>,
    name: String,
    value: Option<serde_json::Value>,
    num_value: Option<f64>,
    str_value: Option<String>,
    data_type: Option<DataType>,
    source: Option<ScoreSource>,
    comment: Option<String>,
    config_id: Option<String>,
    ts_unix_nano: Option<u64>,
}

/// `POST /v1/scores` — upsert one or many scores. Returns the stored ids and a `join`
/// summary of how their targets matched stored spans/traces; `?strict=true` rejects a batch
/// that references a provably-missing span/trace instead of storing an orphaned score.
async fn post_scores(
    State(store): State<Store>,
    Query(q): Query<StrictQuery>,
    body: Bytes,
) -> Response {
    let parsed: ScoresBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("evald: invalid score body: {e}\n"),
            )
                .into_response()
        }
    };
    let inputs = match parsed {
        ScoresBody::One(s) => vec![*s],
        ScoresBody::Many(v) => v,
    };

    let mut scores = Vec::with_capacity(inputs.len());
    let mut hints: Vec<Option<String>> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let hint = input.trace_id.clone();
        match resolve_score(input, ScoreSource::Api) {
            Ok(s) => {
                scores.push(s);
                hints.push(hint);
            }
            Err(msg) => {
                return (StatusCode::BAD_REQUEST, format!("evald: {msg}\n")).into_response()
            }
        }
    }

    let items: Vec<(ScoreTarget, Option<String>)> = scores
        .iter()
        .zip(hints)
        .map(|(s, hint)| (s.target.clone(), hint))
        .collect();
    let (_statuses, summary) = validate_targets(&store, &items);
    let join = summary.to_json(scores.len());
    if q.strict && summary.unmatched > 0 {
        tracing::warn!(
            unmatched = summary.unmatched,
            "rejecting scores: dangling targets"
        );
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "dangling score target(s)", "join": join })),
        )
            .into_response();
    }
    if summary.unmatched > 0 {
        tracing::warn!(
            unmatched = summary.unmatched,
            "stored scores with dangling targets"
        );
    }

    let ids: Vec<&str> = scores.iter().map(|s| s.id.as_str()).collect();
    match store.put_scores(&scores) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "ids": ids, "join": join })),
        )
            .into_response(),
        Err(err) => {
            tracing::error!(%err, "failed to store scores");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "evald: could not store scores\n",
            )
                .into_response()
        }
    }
}

/// `GET /v1/scores?span_id=&trace_id=&session_id=&run_id=&limit=` — scores for a target,
/// or recent scores across all targets when no target is given.
#[derive(serde::Deserialize)]
struct ScoresQuery {
    trace_id: Option<String>,
    span_id: Option<String>,
    session_id: Option<String>,
    run_id: Option<String>,
    limit: Option<usize>,
}

/// Handle `GET /v1/scores` — scores for a target, or recent scores when none is given.
async fn get_scores(State(store): State<Store>, Query(q): Query<ScoresQuery>) -> Response {
    let target = target_from_parts(
        q.span_id.as_deref(),
        q.trace_id.as_deref(),
        q.session_id.as_deref(),
        q.run_id.as_deref(),
    );
    let result = match target {
        Some(t) => store.scores_for_target(&t),
        None => store.list_scores(
            q.limit
                .unwrap_or(DEFAULT_SPAN_LIMIT)
                .clamp(1, MAX_SPAN_LIMIT),
        ),
    };
    match result {
        Ok(scores) => Json(scores).into_response(),
        Err(err) => query_error(err),
    }
}

/// `GET /v1/scores/{id}` — a single score, or 404.
async fn get_score_by_id(State(store): State<Store>, AxPath(id): AxPath<String>) -> Response {
    match store.get_score(&id) {
        Ok(Some(score)) => Json(score).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "evald: no score with that id\n").into_response(),
        Err(err) => query_error(err),
    }
}

// --- SQL query (PoC step 8) ------------------------------------------------------

/// Body of `POST /v1/sql` — a read-only SQL query (`spans` ∪ `scores` tables) + a row cap.
#[derive(serde::Deserialize)]
struct SqlBody {
    sql: String,
    limit: Option<usize>,
}

/// `POST /v1/sql` — run DataFusion SQL over the cold Parquet blocks + the score store and
/// return `{ columns, rows, row_count, truncated }`. A non-read statement or a planning
/// error is a `400` with the message (so the SPA's SQL console can show it).
///
/// Posture: read-only (the [`crate::sql`] guard rejects writes / `EXPLAIN ANALYZE`), but
/// like the rest of the API it has **no authentication** and `evald serve` binds
/// **loopback** by default (`127.0.0.1:4318`). Treat it as untrusted-input-on-a-trusted-
/// network — fine for the laptop / locked-down-CI threat model; put it behind a proxy with
/// authz, and consider the full-scan cost, before exposing `:4318` on a shared network.
async fn post_sql(State(store): State<Store>, body: Bytes) -> Response {
    let req: SqlBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("evald: invalid SQL request body (expected {{\"sql\":\"…\"}}): {e}\n"),
            )
                .into_response()
        }
    };
    let limit = req
        .limit
        .unwrap_or(DEFAULT_SQL_LIMIT)
        .clamp(1, MAX_SQL_LIMIT);
    match crate::sql::query(&store, &req.sql, limit).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("evald: SQL error: {e}\n")).into_response(),
    }
}

/// Phoenix-compatible span-annotation envelope: `{ "data": [ { ... } ] }`.
#[derive(serde::Deserialize)]
struct SpanAnnotationsBody {
    data: Vec<SpanAnnotation>,
}

#[derive(serde::Deserialize)]
struct SpanAnnotation {
    span_id: String,
    name: String,
    annotator_kind: Option<String>, // HUMAN | LLM | CODE
    result: Option<AnnotationResult>,
    /// Optional dedup/upsert key (Phoenix semantics).
    identifier: Option<String>,
}

#[derive(serde::Deserialize)]
struct AnnotationResult {
    label: Option<String>,
    score: Option<f64>,
    explanation: Option<String>,
}

/// `POST /v1/span_annotations` — Phoenix-compatible. Maps each annotation onto a [`Score`]
/// targeting its span and returns `{ "data": [ { "id": ... } ] }`.
async fn post_span_annotations(
    State(store): State<Store>,
    Query(q): Query<StrictQuery>,
    body: Bytes,
) -> Response {
    let parsed: SpanAnnotationsBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => {
            // Phoenix (FastAPI) answers a malformed payload with 422 + a `{"detail":[...]}`
            // body, not 400 + text. Mirror that shape so a Phoenix client's error handling
            // (which reads `detail`) works unchanged against evald.
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "detail": [{
                        "loc": ["body"],
                        "msg": format!("invalid span_annotations body (expected {{\"data\":[...]}}): {e}"),
                        "type": "value_error",
                    }]
                })),
            )
                .into_response();
        }
    };

    let now = now_unix_nano();
    let mut scores = Vec::with_capacity(parsed.data.len());
    for a in parsed.data {
        let r = a.result.unwrap_or(AnnotationResult {
            label: None,
            score: None,
            explanation: None,
        });
        // Upsert by a stable id when an identifier is supplied; else a fresh uuid.
        let id = match &a.identifier {
            Some(ident) if !ident.is_empty() => format!("{}:{}:{}", a.span_id, a.name, ident),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let data_type = if r.score.is_some() {
            DataType::Numeric
        } else if r.label.is_some() {
            DataType::Categorical
        } else {
            DataType::Text
        };
        let source = match a.annotator_kind.as_deref() {
            Some("HUMAN") => ScoreSource::Human,
            _ => ScoreSource::Eval, // LLM / CODE / unspecified
        };
        scores.push(Score {
            id,
            target: ScoreTarget::Span(a.span_id),
            name: a.name,
            num_value: r.score,
            str_value: r.label,
            data_type,
            source,
            comment: r.explanation,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: now,
        });
    }

    // Validate each annotation's span against the store. Annotations carry only a span id
    // (no trace hint), so this is authoritative for hot-tier spans and Unverified otherwise.
    let items: Vec<(ScoreTarget, Option<String>)> =
        scores.iter().map(|s| (s.target.clone(), None)).collect();
    let (_statuses, summary) = validate_targets(&store, &items);
    let join = summary.to_json(scores.len());
    if q.strict && summary.unmatched > 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "detail": [{
                    "loc": ["body", "data"],
                    "msg": format!("{} annotation(s) reference a span not found in the store", summary.unmatched),
                    "type": "value_error",
                }],
                "join": join,
            })),
        )
            .into_response();
    }
    if summary.unmatched > 0 {
        tracing::warn!(
            unmatched = summary.unmatched,
            "stored annotations with dangling spans"
        );
    }

    let ids: Vec<serde_json::Value> = scores
        .iter()
        .map(|s| serde_json::json!({ "id": s.id }))
        .collect();
    match store.put_scores(&scores) {
        Ok(()) => Json(serde_json::json!({ "data": ids, "join": join })).into_response(),
        Err(err) => {
            tracing::error!(%err, "failed to store span annotations");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "evald: could not store annotations\n",
            )
                .into_response()
        }
    }
}

// --- span-join validation (top-20 QW #4) -----------------------------------------
//
// A score/annotation references a span/trace by id. When the id is wrong or the span was
// never ingested, the write still succeeds and the eval/annotation is quietly orphaned —
// the confirmed Phoenix footgun where an id-mismatch is only surfaced with `sync=True`.
// Here every write validates its targets against the store and returns a `join` summary;
// `?strict=true` rejects a batch that references a *provably* missing span/trace.

/// `?strict=true` toggles reject-on-dangling for the score/annotation write paths.
#[derive(serde::Deserialize)]
struct StrictQuery {
    #[serde(default)]
    strict: bool,
}

/// How one score's target matched against stored spans/traces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinStatus {
    /// The referenced span/trace exists.
    Matched,
    /// Provably absent — the trace has no spans, or the span id is not in its (known) trace.
    Unmatched,
    /// Could not be confirmed cheaply (span id given without a trace_id, so only the hot
    /// tier was checked). Never blocks, even in strict mode — absence isn't proven.
    Unverified,
    /// Not span-joinable (a session/run target, or a store read error).
    NotApplicable,
}

/// Tallies of a batch's [`JoinStatus`]es, rendered into the response `join` field.
#[derive(Default)]
struct JoinSummary {
    matched: usize,
    unmatched: usize,
    unverified: usize,
    not_applicable: usize,
}

impl JoinSummary {
    fn record(&mut self, s: JoinStatus) {
        match s {
            JoinStatus::Matched => self.matched += 1,
            JoinStatus::Unmatched => self.unmatched += 1,
            JoinStatus::Unverified => self.unverified += 1,
            JoinStatus::NotApplicable => self.not_applicable += 1,
        }
    }

    fn to_json(&self, checked: usize) -> serde_json::Value {
        let mut warnings = Vec::new();
        if self.unmatched > 0 {
            warnings.push(format!(
                "{} of {} scores reference a span/trace not found in the store \
                 (likely a wrong or not-yet-ingested id)",
                self.unmatched, checked
            ));
        }
        if self.unverified > 0 {
            warnings.push(format!(
                "{} of {} scores target a span id that could not be confirmed \
                 (pass trace_id for an authoritative check)",
                self.unverified, checked
            ));
        }
        serde_json::json!({
            "checked": checked,
            "matched": self.matched,
            "unmatched": self.unmatched,
            "unverified": self.unverified,
            "warnings": warnings,
        })
    }
}

/// Validate each `(target, trace_hint)` against the store, deduping trace reads. `trace_hint`
/// is the `trace_id` posted alongside a span score (absent for Phoenix span-annotations, which
/// carry only a span id). Returns the per-item statuses and their tally.
fn validate_targets(
    store: &Store,
    items: &[(ScoreTarget, Option<String>)],
) -> (Vec<JoinStatus>, JoinSummary) {
    // Fetch each referenced trace at most once. `None` = the read failed (→ Unverified).
    let mut wanted: HashSet<String> = HashSet::new();
    for (target, hint) in items {
        match target {
            ScoreTarget::Trace(id) => {
                wanted.insert(id.clone());
            }
            ScoreTarget::Span(_) => {
                if let Some(h) = hint {
                    wanted.insert(h.clone());
                }
            }
            _ => {}
        }
    }
    let mut trace_spans: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    for tid in wanted {
        let entry = match store.trace(&tid) {
            Ok(spans) => Some(spans.into_iter().map(|s| s.span_id).collect()),
            Err(err) => {
                tracing::warn!(%err, trace_id = %tid, "join-check: trace read failed");
                None
            }
        };
        trace_spans.insert(tid, entry);
    }
    // Hot-tier span ids, for span targets posted without a trace_id (best-effort).
    let hot_ids: HashSet<String> = store.hot_spans().into_iter().map(|s| s.span_id).collect();

    let mut summary = JoinSummary::default();
    let statuses = items
        .iter()
        .map(|(target, hint)| {
            let status = match target {
                ScoreTarget::Trace(id) => match trace_spans.get(id) {
                    Some(Some(set)) if set.is_empty() => JoinStatus::Unmatched,
                    Some(Some(_)) => JoinStatus::Matched,
                    _ => JoinStatus::Unverified,
                },
                ScoreTarget::Span(sid) => match hint.as_deref().and_then(|h| trace_spans.get(h)) {
                    Some(Some(set)) => {
                        if set.contains(sid) {
                            JoinStatus::Matched
                        } else {
                            JoinStatus::Unmatched
                        }
                    }
                    Some(None) => JoinStatus::Unverified,
                    None => {
                        if hot_ids.contains(sid) {
                            JoinStatus::Matched
                        } else {
                            JoinStatus::Unverified
                        }
                    }
                },
                _ => JoinStatus::NotApplicable,
            };
            summary.record(status);
            status
        })
        .collect();
    (statuses, summary)
}

/// Resolve a posted [`ScoreInput`] into a [`Score`], filling in id/ts/value/type.
fn resolve_score(input: ScoreInput, default_source: ScoreSource) -> Result<Score, String> {
    let target = target_from_parts(
        input.span_id.as_deref(),
        input.trace_id.as_deref(),
        input.session_id.as_deref(),
        input.run_id.as_deref(),
    )
    .ok_or("a score needs one of span_id / trace_id / session_id / run_id")?;

    // Resolve the value: explicit num_value/str_value win, else interpret `value`.
    let (mut num_value, mut str_value, mut inferred_type) =
        (input.num_value, input.str_value, None);
    if num_value.is_none() && str_value.is_none() {
        match input.value {
            Some(serde_json::Value::Number(n)) => {
                num_value = n.as_f64();
                inferred_type = Some(DataType::Numeric);
            }
            Some(serde_json::Value::Bool(b)) => {
                num_value = Some(if b { 1.0 } else { 0.0 });
                inferred_type = Some(DataType::Boolean);
            }
            Some(serde_json::Value::String(s)) => {
                str_value = Some(s);
                inferred_type = Some(DataType::Categorical);
            }
            _ => {}
        }
    }
    if num_value.is_none() && str_value.is_none() {
        return Err("a score needs a value (number/string/bool, or num_value/str_value)".into());
    }

    Ok(Score {
        id: input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        target,
        name: input.name,
        num_value,
        str_value,
        data_type: input.data_type.or(inferred_type).unwrap_or_default(),
        source: input.source.unwrap_or(default_source),
        comment: input.comment,
        config_id: input.config_id,
        agg_stats: None,
        ts_unix_nano: input.ts_unix_nano.unwrap_or_else(now_unix_nano),
    })
}

/// Build a [`ScoreTarget`] from the first present id, most-specific (span) first.
fn target_from_parts(
    span_id: Option<&str>,
    trace_id: Option<&str>,
    session_id: Option<&str>,
    run_id: Option<&str>,
) -> Option<ScoreTarget> {
    let non_empty = |s: Option<&str>| s.filter(|v| !v.is_empty()).map(|v| v.to_string());
    if let Some(id) = non_empty(span_id) {
        Some(ScoreTarget::Span(id))
    } else if let Some(id) = non_empty(trace_id) {
        Some(ScoreTarget::Trace(id))
    } else if let Some(id) = non_empty(session_id) {
        Some(ScoreTarget::Session(id))
    } else {
        non_empty(run_id).map(ScoreTarget::Run)
    }
}

/// Current wall-clock time as Unix nanoseconds (0 if the clock predates the epoch).
fn now_unix_nano() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Decode an OTLP-JSON body into the proto request, bridging the int64-as-string gap.
/// `pub` so the fleet ingest gateway (`evald_fleet::fleet::gateway`) can reuse it
/// instead of re-implementing the OTLP-JSON int coercion.
pub fn decode_otlp_json(body: &[u8]) -> Result<ExportTraceServiceRequest, serde_json::Error> {
    let mut value: Json2 = serde_json::from_slice(body)?;
    coerce_otlp_json_ints(&mut value);
    serde_json::from_value(value)
}

/// OTLP-JSON encodes int64 fields as strings (protobuf JSON mapping), but the
/// generated `AnyValue.intValue` / metric `asInt` deserializers expect a number.
/// Recursively rewrite `{"intValue":"123"}` → `{"intValue":123}` so the library can
/// parse a conformant payload. Numbers already in numeric form are left untouched, so
/// both spellings are accepted.
pub fn coerce_otlp_json_ints(value: &mut Json2) {
    match value {
        Json2::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "intValue" || key == "asInt" {
                    if let Json2::String(s) = val {
                        if let Ok(n) = s.parse::<i64>() {
                            *val = Json2::from(n);
                        }
                    }
                }
                coerce_otlp_json_ints(val);
            }
        }
        Json2::Array(items) => {
            for item in items {
                coerce_otlp_json_ints(item);
            }
        }
        _ => {}
    }
}

/// A spec-shaped success reply in the same format as the request. An empty
/// `ExportTraceServiceResponse` (no `partial_success`) means full success.
fn success_response(is_json: bool) -> Response {
    let reply = ExportTraceServiceResponse {
        partial_success: None,
    };
    if is_json {
        let body = serde_json::to_vec(&reply).unwrap_or_else(|_| b"{}".to_vec());
        ([(header::CONTENT_TYPE, CT_JSON)], body).into_response()
    } else {
        ([(header::CONTENT_TYPE, CT_PROTOBUF)], reply.encode_to_vec()).into_response()
    }
}

/// Log a per-request + per-span summary of what was ingested.
fn log_ingest(resource_spans: usize, spans: &[NormalizedSpan]) {
    tracing::info!(
        resource_spans,
        spans = spans.len(),
        "ingesting OTLP/HTTP traces"
    );
    for sp in spans {
        tracing::info!(
            dialect = ?sp.dialect,
            trace_id = %sp.trace_id,
            span_id = %sp.span_id,
            name = %sp.name,
            oi_kind = sp.oi_kind.as_deref().unwrap_or("-"),
            model = sp.model.as_deref().unwrap_or("-"),
            provider = sp.provider.as_deref().unwrap_or("-"),
            prompt_tokens = sp.tokens.prompt.unwrap_or(0),
            completion_tokens = sp.tokens.completion.unwrap_or(0),
            total_tokens = sp.tokens.total.unwrap_or(0),
            cost_usd = sp.cost_usd.unwrap_or(0.0),
            duration_ns = sp.duration_ns(),
            attrs = sp.raw_attributes.len(),
            "span"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use opentelemetry_proto::tonic::common::v1::{any_value::Value as AnyVal, AnyValue, KeyValue};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use tower::ServiceExt; // oneshot

    fn str_kv(key: &str, val: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AnyVal::StringValue(val.to_string())),
            }),
            ..Default::default()
        }
    }

    fn int_kv(key: &str, val: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AnyVal::IntValue(val)),
            }),
            ..Default::default()
        }
    }

    fn dbl_kv(key: &str, val: f64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AnyVal::DoubleValue(val)),
            }),
            ..Default::default()
        }
    }

    fn sample_request() -> ExportTraceServiceRequest {
        let span = Span {
            trace_id: vec![0x11; 16],
            span_id: vec![0x22; 8],
            name: "openai.chat".to_string(),
            kind: 3,
            start_time_unix_nano: 1_000,
            end_time_unix_nano: 4_500,
            attributes: vec![
                str_kv("openinference.span.kind", "LLM"),
                str_kv("gen_ai.request.model", "claude-opus-4-8"),
            ],
            ..Default::default()
        };
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![str_kv("service.name", "checkout")],
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    spans: vec![span],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// A realistic OTLP-JSON payload as an SDK/collector would emit it: hex ids,
    /// string timestamps, and int64 token counts encoded as STRINGS.
    const OTLP_JSON: &str = r#"{
      "resourceSpans": [{
        "resource": { "attributes": [
          { "key": "service.name", "value": { "stringValue": "demo" } }
        ]},
        "scopeSpans": [{
          "scope": { "name": "openinference", "version": "0.1.0" },
          "spans": [{
            "traceId": "0123456789abcdef0123456789abcdef",
            "spanId": "0123456789abcdef",
            "name": "chat",
            "kind": 3,
            "startTimeUnixNano": "1700000000000000000",
            "endTimeUnixNano": "1700000000500000000",
            "attributes": [
              { "key": "openinference.span.kind", "value": { "stringValue": "LLM" } },
              { "key": "gen_ai.request.model", "value": { "stringValue": "gpt-4o" } },
              { "key": "gen_ai.usage.input_tokens", "value": { "intValue": "123" } },
              { "key": "gen_ai.usage.output_tokens", "value": { "intValue": "45" } }
            ]
          }]
        }]
      }]
    }"#;

    fn test_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreConfig::default()).unwrap();
        (store, dir)
    }

    async fn body_bytes(resp: Response) -> Bytes {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body")
    }

    fn post_traces(content_type: &str, body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header(header::CONTENT_TYPE, content_type)
            .body(body.into())
            .unwrap()
    }

    #[test]
    fn protobuf_decode_roundtrips() {
        let req = sample_request();
        let bytes = req.encode_to_vec();
        let decoded = ExportTraceServiceRequest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, req);
    }

    #[test]
    fn otlp_json_decodes_and_normalizes() {
        let req = decode_otlp_json(OTLP_JSON.as_bytes()).expect("decode OTLP-JSON");
        let spans = normalize::normalize_request(&req);
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s.trace_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(s.span_id, "0123456789abcdef");
        assert_eq!(s.start_unix_nano, 1_700_000_000_000_000_000);
        assert_eq!(s.duration_ns(), 500_000_000);
        assert_eq!(s.tokens.prompt, Some(123));
        assert_eq!(s.tokens.completion, Some(45));
        assert_eq!(s.tokens.total, Some(168));
        assert_eq!(s.model.as_deref(), Some("gpt-4o"));
        assert_eq!(s.oi_kind.as_deref(), Some("LLM"));
        assert_eq!(s.service_name.as_deref(), Some("demo"));
        assert_eq!(s.scope_name.as_deref(), Some("openinference"));
    }

    #[test]
    fn coerce_handles_string_and_numeric_int_values() {
        let mut v: Json2 = serde_json::from_str(
            r#"{"a":{"intValue":"7"},"b":{"intValue":9},"c":[{"intValue":"11"}]}"#,
        )
        .unwrap();
        coerce_otlp_json_ints(&mut v);
        assert_eq!(v["a"]["intValue"], Json2::from(7));
        assert_eq!(v["b"]["intValue"], Json2::from(9));
        assert_eq!(v["c"][0]["intValue"], Json2::from(11));
    }

    #[tokio::test]
    async fn post_protobuf_returns_otlp_success() {
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            CT_PROTOBUF
        );
        let bytes = body_bytes(resp).await;
        let reply = ExportTraceServiceResponse::decode(bytes.as_ref()).expect("decode reply");
        assert!(reply.partial_success.is_none());
    }

    #[tokio::test]
    async fn post_otlp_json_returns_success() {
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(post_traces(CT_JSON, OTLP_JSON))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), CT_JSON);
    }

    // A full Phoenix-shaped span-annotation payload: the LLM/CODE/HUMAN annotator kind, the
    // result{label,score,explanation}, optional metadata (accepted + ignored — evald has no
    // metadata field), and the identifier upsert key. Locks the Phoenix REST contract.
    const PHOENIX_ANNOTATION: &str = r#"{"data":[{
        "span_id":"0123456789abcdef",
        "name":"correctness",
        "annotator_kind":"HUMAN",
        "result":{"label":"correct","score":1.0,"explanation":"looks right"},
        "metadata":{"reviewer":"alice"},
        "identifier":"rev-1"
    }]}"#;

    #[tokio::test]
    async fn span_annotations_phoenix_envelope_roundtrips_and_upserts() {
        let (store, _dir) = test_store();

        let resp = router(store.clone())
            .oneshot(post_json("/v1/span_annotations", PHOENIX_ANNOTATION))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Response shape is exactly Phoenix's: {"data":[{"id":"..."}]}.
        let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(
            v["data"][0]["id"].as_str(),
            Some("0123456789abcdef:correctness:rev-1") // identifier-derived upsert key
        );

        // It landed as a span-targeted Score with the result mapped through.
        let scores = store
            .scores_for_target(&ScoreTarget::Span("0123456789abcdef".into()))
            .unwrap();
        assert_eq!(scores.len(), 1);
        let s = &scores[0];
        assert_eq!(s.name, "correctness");
        assert_eq!(s.num_value, Some(1.0));
        assert_eq!(s.str_value.as_deref(), Some("correct"));
        assert_eq!(s.comment.as_deref(), Some("looks right"));
        assert!(matches!(s.source, ScoreSource::Human));

        // Re-posting the same identifier upserts (Phoenix semantics) — no duplicate row.
        let resp2 = router(store.clone())
            .oneshot(post_json("/v1/span_annotations", PHOENIX_ANNOTATION))
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        assert_eq!(
            store
                .scores_for_target(&ScoreTarget::Span("0123456789abcdef".into()))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn span_annotations_malformed_body_is_422_with_detail() {
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(post_json("/v1/span_annotations", "{ not json"))
            .await
            .unwrap();
        // Phoenix/FastAPI validation-error contract: 422 + {"detail":[{...}]}.
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert!(v["detail"][0]["msg"].is_string(), "detail.msg missing: {v}");
    }

    #[tokio::test]
    async fn post_garbage_protobuf_is_bad_request() {
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(post_traces(CT_PROTOBUF, vec![0xff, 0xff, 0xff, 0xff]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_garbage_json_is_bad_request() {
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(post_traces(CT_JSON, "{ not json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ingest_then_query_roundtrip() {
        let (store, _dir) = test_store();
        let app = router(store);

        let resp = app
            .clone()
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let get = Request::builder()
            .method("GET")
            .uri("/v1/spans")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let spans: Vec<NormalizedSpan> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].trace_id, "11".repeat(16));
        assert_eq!(spans[0].oi_kind.as_deref(), Some("LLM"));
        assert_eq!(spans[0].model.as_deref(), Some("claude-opus-4-8"));

        // The sample LLM span carries no token usage, so the read API flags it with the
        // `usage_missing` diagnostic instead of silently reporting 0 tokens.
        let raw: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(raw[0]["usage_missing"], serde_json::json!("no-usage-field"));
    }

    #[tokio::test]
    async fn spans_with_usage_omit_the_diagnostic_key() {
        // A span that DOES report tokens must not grow a `usage_missing` key — the wire
        // shape stays unchanged for the healthy common case.
        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_JSON, OTLP_JSON.as_bytes().to_vec()))
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/spans")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let raw: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(raw.len(), 1);
        assert!(
            raw[0].get("usage_missing").is_none(),
            "span with usage should have no diagnostic: {}",
            raw[0]
        );
    }

    #[tokio::test]
    async fn get_trace_filters_and_404s() {
        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();

        let tid = "11".repeat(16);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/v1/traces/{tid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spans: Vec<NormalizedSpan> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(spans.len(), 1);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/traces/deadbeef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// A trace with a valid parent chain plus one span whose parent is not in the trace
    /// (a dropped/sampled intermediate) — only the dangling span is flagged `orphan_parent`,
    /// and only on the full-trace read.
    #[tokio::test]
    async fn get_trace_flags_broken_nesting() {
        let tid = vec![0x33; 16];
        let root = Span {
            trace_id: tid.clone(),
            span_id: vec![0xa1; 8],
            name: "root".to_string(),
            start_time_unix_nano: 1_000,
            end_time_unix_nano: 9_000,
            ..Default::default()
        };
        // Valid child — parent IS the root, so it must NOT be flagged.
        let child = Span {
            trace_id: tid.clone(),
            span_id: vec![0xb2; 8],
            parent_span_id: vec![0xa1; 8],
            name: "child".to_string(),
            start_time_unix_nano: 2_000,
            end_time_unix_nano: 3_000,
            ..Default::default()
        };
        // Orphan — parent 0xc3.. is absent from the trace, so it MUST be flagged.
        let orphan = Span {
            trace_id: tid.clone(),
            span_id: vec![0xd4; 8],
            parent_span_id: vec![0xc3; 8],
            name: "orphan".to_string(),
            start_time_unix_nano: 4_000,
            end_time_unix_nano: 5_000,
            ..Default::default()
        };
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![root, child, orphan],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };

        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, req.encode_to_vec()))
            .await
            .unwrap();

        let hex_tid = "33".repeat(16);
        let resp = app
            .oneshot(get(&format!("/v1/traces/{hex_tid}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spans: Vec<serde_json::Value> =
            serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(spans.len(), 3);

        let by_name = |name: &str| {
            spans
                .iter()
                .find(|s| s["name"] == name)
                .unwrap_or_else(|| panic!("missing span {name}"))
        };
        assert!(
            by_name("root").get("orphan_parent").is_none(),
            "root span has no parent, must not be flagged"
        );
        assert!(
            by_name("child").get("orphan_parent").is_none(),
            "child's parent is present, must not be flagged"
        );
        assert_eq!(
            by_name("orphan")["orphan_parent"],
            serde_json::Value::Bool(true),
            "span with a missing parent must be flagged"
        );
    }

    /// The full-trace diagnostic is context-dependent: the SAME span served from the paged
    /// `/v1/spans` read (no full-trace context) must NOT be flagged, since a missing parent
    /// there may simply be on another page / in cold storage.
    #[tokio::test]
    async fn get_spans_does_not_flag_orphan_parent() {
        let orphan = Span {
            trace_id: vec![0x44; 16],
            span_id: vec![0xd4; 8],
            parent_span_id: vec![0xc3; 8],
            name: "orphan".to_string(),
            start_time_unix_nano: 4_000,
            end_time_unix_nano: 5_000,
            ..Default::default()
        };
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![orphan],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, req.encode_to_vec()))
            .await
            .unwrap();

        let resp = app.oneshot(get("/v1/spans")).await.unwrap();
        let spans: Vec<serde_json::Value> =
            serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].get("orphan_parent").is_none(),
            "the paged span read must not run the full-trace diagnostic"
        );
    }

    /// A session rollup sums cost + tokens + span/trace counts over every span carrying the
    /// session id, across multiple traces, and ignores spans from other sessions.
    #[tokio::test]
    async fn session_rollup_aggregates_across_traces() {
        fn llm_span(trace: u8, span: u8, session: &str, cost: f64, prompt: i64, comp: i64) -> Span {
            Span {
                trace_id: vec![trace; 16],
                span_id: vec![span; 8],
                name: "chat".to_string(),
                start_time_unix_nano: 1_000 * (span as u64),
                end_time_unix_nano: 1_000 * (span as u64) + 500,
                attributes: vec![
                    str_kv("openinference.span.kind", "LLM"),
                    str_kv("session.id", session),
                    dbl_kv("llm.cost.total", cost),
                    int_kv("llm.token_count.prompt", prompt),
                    int_kv("llm.token_count.completion", comp),
                ],
                ..Default::default()
            }
        }
        // Two spans of session "sess-9" in two different traces, plus one span of another
        // session that must be excluded from the rollup.
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![
                        llm_span(0x01, 0x11, "sess-9", 0.001, 100, 50),
                        llm_span(0x02, 0x22, "sess-9", 0.002, 200, 60),
                        llm_span(0x03, 0x33, "other", 9.999, 999, 999),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, req.encode_to_vec()))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(get("/v1/sessions/sess-9"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let roll = json_body(resp).await;
        assert_eq!(roll["session_id"], "sess-9");
        assert_eq!(roll["span_count"], 2);
        assert_eq!(roll["trace_count"], 2);
        // 0.001 + 0.002, tolerant of float representation.
        assert!(
            (roll["cost_usd"].as_f64().unwrap() - 0.003).abs() < 1e-9,
            "cost rollup: {}",
            roll["cost_usd"]
        );
        assert_eq!(roll["tokens"]["prompt"], 300);
        assert_eq!(roll["tokens"]["completion"], 110);
        assert_eq!(roll["start_unix_nano"], 1_000 * 0x11);
        assert_eq!(roll["end_unix_nano"], 1_000 * 0x22 + 500);
        assert_eq!(roll["truncated"], false);

        // Unknown session → 404.
        let resp = app.oneshot(get("/v1/sessions/nope")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stats_endpoint_reports_hot_backlog() {
        let (store, _dir) = test_store();
        let app = router(store);
        // One span ingested → backlog of 1, healthy (not shedding, no rejections).
        app.clone()
            .oneshot(post_traces(CT_JSON, OTLP_JSON.as_bytes().to_vec()))
            .await
            .unwrap();

        let resp = app.oneshot(get("/v1/stats")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let stats = json_body(resp).await;
        assert_eq!(stats["hot_spans"], 1);
        assert_eq!(stats["shedding"], false);
        assert_eq!(stats["rejections"], 0);
        assert!(stats["channel_capacity"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn oversized_payload_is_offloaded_and_fetchable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                blob_offload_bytes: 1_024, // small cap so a modest test payload trips it
                ..StoreConfig::default()
            },
        )
        .unwrap();
        let app = router(store);

        let big = "R".repeat(5_000);
        let span = Span {
            trace_id: vec![0x55; 16],
            span_id: vec![0x66; 8],
            name: "chat".to_string(),
            attributes: vec![
                str_kv("openinference.span.kind", "LLM"),
                str_kv("input.value", &big),
            ],
            ..Default::default()
        };
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![span],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, req.encode_to_vec()))
            .await
            .unwrap();

        // Read back: input_value is a compact reference, not the 5k payload — and the raw
        // `input.value` attribute was offloaded too, so the Parquet raw column stays lean.
        let resp = app.clone().oneshot(get("/v1/spans")).await.unwrap();
        let spans: Vec<serde_json::Value> =
            serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(spans.len(), 1);
        let reference = spans[0]["input_value"].as_str().unwrap().to_string();
        assert!(reference.starts_with("evald-blob:"), "got {reference}");
        assert_eq!(
            spans[0]["raw_attributes"]["evald.blob.input"]["bytes"],
            5_000
        );
        let raw_input = spans[0]["raw_attributes"]["input.value"].as_str().unwrap();
        assert!(
            raw_input.starts_with("evald-blob:"),
            "raw input.value must be offloaded too, got {raw_input}"
        );

        // Fetch the blob → the original bytes come back verbatim.
        let key = reference.strip_prefix("evald-blob:").unwrap();
        let resp = app
            .clone()
            .oneshot(get(&format!("/v1/blobs/{key}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        assert_eq!(bytes.len(), 5_000);
        assert_eq!(&bytes[..], big.as_bytes());

        // Unknown blob → 404.
        let resp = app.oneshot(get("/v1/blobs/deadbeef")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- scores ------------------------------------------------------------------

    fn post_json(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, CT_JSON)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    async fn json_body(resp: Response) -> serde_json::Value {
        serde_json::from_slice(&body_bytes(resp).await).unwrap()
    }

    #[tokio::test]
    async fn post_score_then_query_by_span() {
        let (store, _dir) = test_store();
        let app = router(store);
        let resp = app
            .clone()
            .oneshot(post_json(
                "/v1/scores",
                r#"{"span_id":"aa","name":"exact_match","value":1,"source":"eval"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["ids"].as_array().unwrap().len(), 1);

        let resp = app.oneshot(get("/v1/scores?span_id=aa")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let scores = json_body(resp).await;
        assert_eq!(scores[0]["name"], "exact_match");
        assert_eq!(scores[0]["num_value"], 1.0);
        assert_eq!(scores[0]["target_type"], "span");
        assert_eq!(scores[0]["target_id"], "aa");
        assert_eq!(scores[0]["data_type"], "numeric"); // inferred from a numeric value
        assert_eq!(scores[0]["source"], "eval");
    }

    #[tokio::test]
    async fn post_scores_array_and_categorical_value() {
        let (store, _dir) = test_store();
        let app = router(store);
        let resp = app
            .clone()
            .oneshot(post_json(
                "/v1/scores",
                r#"[{"trace_id":"tt","name":"helpfulness","value":"good"},
                    {"span_id":"sp","name":"latency","value":1234}]"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(json_body(resp).await["ids"].as_array().unwrap().len(), 2);

        let resp = app.oneshot(get("/v1/scores?trace_id=tt")).await.unwrap();
        let scores = json_body(resp).await;
        assert_eq!(scores[0]["str_value"], "good");
        assert_eq!(scores[0]["data_type"], "categorical"); // inferred from a string value
    }

    #[tokio::test]
    async fn score_targeting_an_ingested_span_reports_matched() {
        let (store, _dir) = test_store();
        let app = router(store);
        // Ingest trace 11..×16 with span 22..×8 (from `sample_request`).
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();
        let tid = "11".repeat(16);
        let sid = "22".repeat(8);
        let body = format!(r#"{{"trace_id":"{tid}","span_id":"{sid}","name":"q","value":1}}"#);
        let resp = app.oneshot(post_json("/v1/scores", &body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let j = json_body(resp).await;
        assert_eq!(j["join"]["matched"], 1);
        assert_eq!(j["join"]["unmatched"], 0);
    }

    #[tokio::test]
    async fn strict_mode_rejects_a_dangling_span_reference() {
        let (store, _dir) = test_store();
        let app = router(store);
        app.clone()
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();
        let tid = "11".repeat(16);
        // A span id that is NOT in that (existing) trace → provably Unmatched.
        let body =
            format!(r#"{{"trace_id":"{tid}","span_id":"deadbeefdeadbeef","name":"q","value":1}}"#);
        let resp = app
            .clone()
            .oneshot(post_json("/v1/scores?strict=true", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let j = json_body(resp).await;
        assert_eq!(j["join"]["unmatched"], 1);

        // Without strict, the same write is stored but the dangling reference is surfaced.
        let resp = app.oneshot(post_json("/v1/scores", &body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let j = json_body(resp).await;
        assert_eq!(j["join"]["unmatched"], 1);
        assert!(!j["join"]["warnings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn span_only_score_without_trace_hint_is_unverified_not_unmatched() {
        // A span id alone (no trace_id, no ingest) can't be proven absent, so it is reported
        // as unverified — and strict mode must NOT reject it (absence is not proven).
        let (store, _dir) = test_store();
        let app = router(store);
        let resp = app
            .oneshot(post_json(
                "/v1/scores?strict=true",
                r#"{"span_id":"ff","name":"q","value":1}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let j = json_body(resp).await;
        assert_eq!(j["join"]["unverified"], 1);
        assert_eq!(j["join"]["unmatched"], 0);
    }

    #[tokio::test]
    async fn post_score_without_target_or_value_is_400() {
        let (store, _dir) = test_store();
        let app = router(store);
        let no_target = app
            .clone()
            .oneshot(post_json("/v1/scores", r#"{"name":"x","value":1}"#))
            .await
            .unwrap();
        assert_eq!(no_target.status(), StatusCode::BAD_REQUEST);
        let no_value = app
            .oneshot(post_json("/v1/scores", r#"{"span_id":"aa","name":"x"}"#))
            .await
            .unwrap();
        assert_eq!(no_value.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn span_annotations_phoenix_envelope_maps_to_scores() {
        let (store, _dir) = test_store();
        let app = router(store);
        let body = r#"{"data":[{
            "span_id":"abc","name":"correctness","annotator_kind":"HUMAN",
            "result":{"label":"correct","score":0.9,"explanation":"looks right"},
            "identifier":"run-1"
        }]}"#;
        let resp = app
            .clone()
            .oneshot(post_json("/v1/span_annotations", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let out = json_body(resp).await;
        let id = out["data"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(id, "abc:correctness:run-1"); // upsert id derived from identifier

        // The annotation is retrievable as a score on the span.
        let resp = app
            .clone()
            .oneshot(get("/v1/scores?span_id=abc"))
            .await
            .unwrap();
        let scores = json_body(resp).await;
        assert_eq!(scores[0]["name"], "correctness");
        assert_eq!(scores[0]["num_value"], 0.9);
        assert_eq!(scores[0]["str_value"], "correct");
        assert_eq!(scores[0]["comment"], "looks right");
        assert_eq!(scores[0]["source"], "human");

        // GET /v1/scores/{id}
        let resp = app
            .clone()
            .oneshot(get(&format!("/v1/scores/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["id"], id);

        // Re-POST with the same identifier upserts (no duplicate).
        app.clone()
            .oneshot(post_json("/v1/span_annotations", body))
            .await
            .unwrap();
        let resp = app.oneshot(get("/v1/scores?span_id=abc")).await.unwrap();
        assert_eq!(json_body(resp).await.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_unknown_score_is_404() {
        let (store, _dir) = test_store();
        let resp = router(store).oneshot(get("/v1/scores/nope")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn body_limit_caps_decompressed_size_not_compressed() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write as _;

        // A tiny gzip that inflates well past the limit — the classic decompression bomb.
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&vec![0u8; 8192]).unwrap(); // 8 KiB decompressed
        let gz = enc.finish().unwrap();
        assert!(
            gz.len() < 1024,
            "gzip of zeros is tiny ({} bytes)",
            gz.len()
        );

        let (store, _dir) = test_store();
        let app = build_router(store, 1024, Auth::disabled()); // 1 KiB decompressed cap
        let req = Request::builder()
            .method("POST")
            .uri("/v1/traces")
            .header(header::CONTENT_TYPE, CT_PROTOBUF)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(gz))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // 413 means the cap was enforced on the INFLATED body. (With the limit outside
        // decompression it would cap the tiny compressed body, let the bomb through, and
        // fail later at protobuf decode with 400.)
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // --- bearer-token auth gate ------------------------------------------------------

    const TEST_TOKEN: &str = "test-token-0123456789";

    /// A GET with an optional `Authorization: Bearer <token>` header.
    fn get_bearer(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    fn auth_router(store: Store) -> Router {
        router_with_auth(store, Auth::from_tokens([TEST_TOKEN]).unwrap())
    }

    #[tokio::test]
    async fn auth_gate_rejects_missing_and_wrong_tokens_with_401() {
        let (store, _dir) = test_store();
        let app = auth_router(store);

        // No Authorization header → 401 + a Bearer challenge.
        let resp = app
            .clone()
            .oneshot(get_bearer("/v1/spans", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );

        // Wrong token → 401.
        let resp = app
            .clone()
            .oneshot(get_bearer("/v1/spans", Some("wrong-token-0123456789")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Correct token → the request is served (200).
        let resp = app
            .oneshot(get_bearer("/v1/spans", Some(TEST_TOKEN)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_gate_guards_ingest_writes() {
        let (store, _dir) = test_store();
        let app = auth_router(store);

        // Unauthenticated ingest is blocked BEFORE the body is decoded — 401, not 200.
        let resp = app
            .clone()
            .oneshot(post_traces(CT_PROTOBUF, sample_request().encode_to_vec()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // With a valid token the same ingest succeeds.
        let mut req = post_traces(CT_PROTOBUF, sample_request().encode_to_vec());
        req.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {TEST_TOKEN}").parse().unwrap(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_gate_also_guards_the_spa_fallback() {
        // Nothing is reachable without a token when the gate is armed — including the
        // embedded SPA served by the router fallback.
        let (store, _dir) = test_store();
        let app = auth_router(store);
        let resp = app.oneshot(get_bearer("/", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn disabled_auth_serves_without_a_token() {
        // The default (no-auth) router is unchanged: no Authorization header, still 200.
        let (store, _dir) = test_store();
        let resp = router(store)
            .oneshot(get_bearer("/v1/spans", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
