//! OTLP logs in: `POST /v1/logs` and the gRPC `LogsService` (0.4.0, `ROADMAP-0.4-0.6.md` 4.1).
//!
//! Logs are how coding agents send most of what they know: Claude Code and Codex emit their
//! per-request facts (model, tokens, cost, duration, tool decisions) as OTLP **log events** and
//! keep traces opt-in. It is also the transport the OTel GenAI conventions give
//! `gen_ai.evaluation.result`.
//!
//! # evald is not a log store
//!
//! A record is kept only when it is recognised, and then it lands in the model evald already has:
//!
//! * a `gen_ai.evaluation.result` event becomes a [`Score`] on the span its trace context names
//!   (the same mapping as the span-event form, [`crate::evalevent`]);
//! * an event from a mapped dialect becomes a span: a request event an LLM span, a tool event a
//!   TOOL span (the dialects are listed in [`map_dialect`]).
//!
//! Everything else is acknowledged, counted by outcome (`evald_log_records_total{outcome}`) and
//! dropped. `--logs all` keeps every unrecognised record too, as a zero-duration span carrying the
//! record's attributes, body and severity; `--logs off` drops every record. Nothing here adds a
//! column or changes the on-disk format: everything kept is an ordinary span or score.
//!
//! # Event names
//!
//! A record's event name is read from the `event.name` attribute (older SDKs, Claude Code, Codex,
//! Gemini CLI), then the OTLP `event_name` field, then a string body. See [`event_name`].
//!
//! # Records that arrive without their context
//!
//! A coding agent reports a model call on two records: the request (which carries the trace
//! context) and, when the stream ends, the usage (which, for Codex and Gemini CLI, does not). An
//! exporter flushes on a timer, so a call that streams for longer than that puts the two in
//! different export requests. [`Recall`] is what a dialect remembers about a session between
//! records to place the second: bounded, per node, and consulted only for where a span sits and
//! when it started. Tokens, cost and session ids never depend on it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value as AnyVal, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use opentelemetry_proto::tonic::trace::v1::{status::StatusCode, Span, Status};
use sha2::{Digest, Sha256};

use crate::model::{NormalizedSpan, Score};
use crate::normalize::{attr_str, normalize_span};

/// What the logs door keeps. `--logs` / `EVALD_LOGS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogsPolicy {
    /// Accept every request and keep nothing.
    Off,
    /// Keep recognised records: evaluation results and mapped dialects. The default.
    #[default]
    Genai,
    /// Also keep every unrecognised record, as a zero-duration span.
    All,
}

impl FromStr for LogsPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "genai" => Ok(Self::Genai),
            "all" => Ok(Self::All),
            other => Err(format!(
                "unknown --logs policy {other:?}: expected genai, all or off"
            )),
        }
    }
}

impl fmt::Display for LogsPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Genai => "genai",
            Self::All => "all",
        })
    }
}

/// What happened to one log record. Each record lands in exactly one outcome, so the outcomes
/// sum to the records received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Became a span (a mapped dialect event, or any record under `--logs all`).
    Span,
    /// Became a score (`gen_ai.evaluation.result`).
    Score,
    /// Recognised, and deliberately not stored on its own because another record carries the
    /// same facts (for example an accepted tool decision, which the tool result covers).
    Folded,
    /// Not recognised, and dropped under `--logs genai`.
    Unmapped,
    /// Dropped because the policy is `--logs off`.
    Off,
    /// A `gen_ai.evaluation.result` record too malformed to be a score.
    Malformed,
}

impl Outcome {
    pub const ALL: [Outcome; 6] = [
        Outcome::Span,
        Outcome::Score,
        Outcome::Folded,
        Outcome::Unmapped,
        Outcome::Off,
        Outcome::Malformed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Span => "span",
            Outcome::Score => "score",
            Outcome::Folded => "folded",
            Outcome::Unmapped => "unmapped",
            Outcome::Off => "off",
            Outcome::Malformed => "malformed",
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

static OUTCOMES: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Records seen per outcome since the process started, for `/metrics`. Counted when a request
/// is mapped, before it is stored: a request the store then refuses (503) is counted again
/// when the exporter retries it.
pub fn outcome_counts() -> [(Outcome, u64); 6] {
    Outcome::ALL.map(|o| (o, OUTCOMES[o.index()].load(Ordering::Relaxed)))
}

/// One export request, mapped.
#[derive(Debug, Default)]
pub struct LogsBatch {
    pub spans: Vec<NormalizedSpan>,
    pub scores: Vec<Score>,
    /// Records in the request, whatever became of them.
    pub records: usize,
    /// Per-outcome counts for this request, in [`Outcome::ALL`] order.
    pub outcomes: [u64; 6],
}

impl LogsBatch {
    fn note(&mut self, o: Outcome) {
        self.outcomes[o.index()] += 1;
    }
}

/// The resource and scope a record arrived under.
pub(crate) struct RecordCtx<'a> {
    pub service_name: Option<String>,
    pub scope_name: Option<&'a str>,
    pub scope_version: Option<&'a str>,
}

/// What a dialect made of one record.
pub(crate) enum Mapping {
    /// One span to store.
    Span(Box<NormalizedSpan>),
    /// Recognised, covered by another record; not stored.
    Folded,
    /// Not this dialect's record (or an event it does not map).
    NotMine,
}

/// Map an export request under `policy`. Never fails: an unrecognised or malformed record is
/// counted and skipped, and the rest of the request is unaffected.
pub fn normalize_logs(req: &ExportLogsServiceRequest, policy: LogsPolicy) -> LogsBatch {
    let mut batch = LogsBatch::default();
    // One table per request (an `Arc` clone), as on the traces path.
    let prices = crate::price::current();
    let mut eval_bad = 0u64;
    let mut state = RequestState::default();
    for resource_logs in &req.resource_logs {
        let service_name = resource_logs
            .resource
            .as_ref()
            .and_then(|r| attr_str(&r.attributes, "service.name"));
        for scope_logs in &resource_logs.scope_logs {
            let (scope_name, scope_version) = match &scope_logs.scope {
                Some(s) => (non_empty(&s.name), non_empty(&s.version)),
                None => (None, None),
            };
            let ctx = RecordCtx {
                service_name: service_name.clone(),
                scope_name,
                scope_version,
            };
            for rec in &scope_logs.log_records {
                batch.records += 1;
                if policy == LogsPolicy::Off {
                    batch.note(Outcome::Off);
                    continue;
                }
                let name = event_name(rec);
                if name.as_deref() == Some(crate::evalevent::EVENT_NAME) {
                    let (trace, span) = trace_context(rec);
                    let t = record_time(rec);
                    match crate::evalevent::score_from_event(
                        trace.unwrap_or_default(),
                        span.unwrap_or_default(),
                        &rec.attributes,
                        t,
                        t,
                    ) {
                        Some(score) => {
                            batch.scores.push(score);
                            batch.note(Outcome::Score);
                        }
                        None => {
                            eval_bad += 1;
                            batch.note(Outcome::Malformed);
                        }
                    }
                    continue;
                }
                match map_dialect(&ctx, rec, name.as_deref(), &mut state) {
                    Mapping::Span(mut span) => {
                        crate::price::apply(&prices, &mut span);
                        batch.spans.push(*span);
                        batch.note(Outcome::Span);
                    }
                    Mapping::Folded => batch.note(Outcome::Folded),
                    Mapping::NotMine if policy == LogsPolicy::All => {
                        let mut span = generic_span(&ctx, rec, name.as_deref());
                        crate::price::apply(&prices, &mut span);
                        batch.spans.push(span);
                        batch.note(Outcome::Span);
                    }
                    Mapping::NotMine => batch.note(Outcome::Unmapped),
                }
            }
        }
    }
    crate::evalevent::count(batch.outcomes[Outcome::Score.index()] as usize, eval_bad);
    for o in Outcome::ALL {
        let n = batch.outcomes[o.index()];
        if n > 0 {
            OUTCOMES[o.index()].fetch_add(n, Ordering::Relaxed);
        }
    }
    batch
}

/// The dialects that turn records into spans, tried in order. Each recognises its own records
/// (by scope or service name) and answers [`Mapping::NotMine`] for everything else.
///
/// * Claude Code ([`crate::claude_code`]).
/// * Codex CLI ([`crate::codex`]).
/// * Gemini CLI ([`crate::gemini_cli`]).
fn map_dialect(
    ctx: &RecordCtx,
    rec: &LogRecord,
    name: Option<&str>,
    state: &mut RequestState,
) -> Mapping {
    match crate::claude_code::map_log(ctx, rec, name) {
        Mapping::NotMine => {}
        m => return m,
    }
    match crate::codex::map_log(ctx, rec, name, &mut state.codex) {
        Mapping::NotMine => {}
        m => return m,
    }
    crate::gemini_cli::map_log(ctx, rec, name, &mut state.gemini)
}

/// What the dialects remember while one export request is mapped: the first layer of each
/// dialect's [`Recall`].
#[derive(Default)]
pub(crate) struct RequestState {
    pub codex: HashMap<String, crate::codex::Conversation>,
    pub gemini: HashMap<String, crate::gemini_cli::Session>,
}

/// What a dialect remembers about each session between records (module docs, *Records that
/// arrive without their context*). Two layers:
///
/// * the request being mapped (a `HashMap` the caller owns): looked at first. A request is
///   mapped in order, so within it this layer is exact, and it is shared with nothing;
/// * the [`RECALL_CAPACITY`] sessions most recently seen on this node, across requests.
///
/// It is a hint, not a ledger: a restart, a second node, or two exporters of one session
/// interleaving can each make it wrong or empty, and a dialect then falls back to the placement
/// a record gives on its own.
pub(crate) struct Recall<T> {
    recent: Mutex<RecallInner<T>>,
}

struct RecallInner<T> {
    map: HashMap<String, T>,
    order: VecDeque<String>,
}

/// Sessions a [`Recall`] remembers across requests before forgetting the oldest.
pub const RECALL_CAPACITY: usize = 4_096;

impl<T: Clone + Default> Recall<T> {
    pub fn new() -> Self {
        Self {
            recent: Mutex::new(RecallInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// What is remembered about `session`: this request's copy, else this node's. A record with
    /// no session id is remembered within its request only: across requests, every such record
    /// would share one entry.
    pub fn get(&self, local: &HashMap<String, T>, session: &str) -> Option<T> {
        if let Some(t) = local.get(session) {
            return Some(t.clone());
        }
        if session.is_empty() {
            return None;
        }
        let recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.map.get(session).cloned()
    }

    /// Change what is remembered about `session`, in this request and on this node.
    pub fn update(&self, local: &mut HashMap<String, T>, session: &str, f: impl FnOnce(&mut T)) {
        if session.is_empty() {
            f(local.entry(String::new()).or_default());
            return;
        }
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let entry = local
            .entry(session.to_string())
            .or_insert_with(|| recent.map.get(session).cloned().unwrap_or_default());
        f(entry);
        let value = entry.clone();
        if recent.map.insert(session.to_string(), value).is_none() {
            recent.order.push_back(session.to_string());
            while recent.order.len() > RECALL_CAPACITY {
                if let Some(old) = recent.order.pop_front() {
                    recent.map.remove(&old);
                }
            }
        }
    }
}

/// A record's event name: the `event.name` attribute, else the OTLP `event_name` field, else a
/// string body. `None` when none is set.
///
/// The attribute comes first because the field is not always an event name: Rust's
/// `opentelemetry-appender-tracing` fills it with the `tracing` callsite (Codex 0.163.0 sends
/// `event otel/src/events/session_telemetry.rs:1132`, with the real name in `event.name`). An
/// emitter that sets both to a real name sets them alike.
pub(crate) fn event_name(rec: &LogRecord) -> Option<String> {
    if let Some(n) = attr_str(&rec.attributes, "event.name").filter(|n| !n.is_empty()) {
        return Some(n);
    }
    if !rec.event_name.is_empty() {
        return Some(rec.event_name.clone());
    }
    match rec.body.as_ref().and_then(|b| b.value.as_ref()) {
        Some(AnyVal::StringValue(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The record's trace and span id, each `None` when absent or all zero.
pub(crate) fn trace_context(rec: &LogRecord) -> (Option<&[u8]>, Option<&[u8]>) {
    let set = |b: &[u8], len: usize| (b.len() == len && b.iter().any(|&x| x != 0)).then_some(());
    (
        set(&rec.trace_id, 16).map(|()| rec.trace_id.as_slice()),
        set(&rec.span_id, 8).map(|()| rec.span_id.as_slice()),
    )
}

/// When the record happened: its own time, else when the SDK observed it.
pub(crate) fn record_time(rec: &LogRecord) -> u64 {
    if rec.time_unix_nano > 0 {
        rec.time_unix_nano
    } else {
        rec.observed_time_unix_nano
    }
}

/// A deterministic id of `len` bytes from `parts`. The same inputs give the same id on every
/// node and every retry, which is what lets two signals describing one call land on one span.
pub(crate) fn derive_id(len: usize, parts: &[&str]) -> Vec<u8> {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    let mut id = h.finalize()[..len].to_vec();
    // An all-zero id means "absent" in OTLP; astronomically unlikely, but never emit one.
    if id.iter().all(|&b| b == 0) {
        id[0] = 1;
    }
    id
}

/// The span a mapped record becomes, before [`normalize_span`] reads it.
pub(crate) struct SpanDraft {
    pub trace_id: Vec<u8>,
    pub span_id: Vec<u8>,
    pub parent_span_id: Vec<u8>,
    pub name: String,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    /// `None` = unset; `Some(Err(msg))` = an error with that message.
    pub status: Option<Result<(), String>>,
    pub attributes: Vec<KeyValue>,
}

impl SpanDraft {
    /// Run the draft through the same normalisation a received span gets, so model, provider,
    /// tokens, cost, session and kind are read by one implementation.
    pub fn finish(self, ctx: &RecordCtx) -> NormalizedSpan {
        let status = self.status.map(|s| match s {
            Ok(()) => Status {
                code: StatusCode::Ok as i32,
                message: String::new(),
            },
            Err(message) => Status {
                code: StatusCode::Error as i32,
                message,
            },
        });
        let span = Span {
            trace_id: self.trace_id,
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
            name: self.name,
            // INTERNAL: the record says nothing about client/server.
            kind: 1,
            start_time_unix_nano: self.start_unix_nano,
            end_time_unix_nano: self.end_unix_nano,
            attributes: self.attributes,
            status,
            ..Default::default()
        };
        normalize_span(
            &span,
            ctx.service_name.clone(),
            ctx.scope_name.map(str::to_string),
            ctx.scope_version.map(str::to_string),
        )
    }
}

/// `--logs all`: an unrecognised record as a zero-duration span at the record's time, carrying
/// its attributes plus `log.body`, `log.severity_text` and `log.severity_number`.
fn generic_span(ctx: &RecordCtx, rec: &LogRecord, name: Option<&str>) -> NormalizedSpan {
    let t = record_time(rec);
    let (trace, parent) = trace_context(rec);
    let t_str = t.to_string();
    let n = name.unwrap_or("log");
    let body_digest = rec
        .body
        .as_ref()
        .map(|b| format!("{b:?}"))
        .unwrap_or_default();
    // Without a trace context, a trace of its own: one per service would grow without bound
    // with every plain log line. Derived from what the span id is, so a retry lands in it.
    let trace_id = match trace {
        Some(t) => t.to_vec(),
        None => derive_id(
            16,
            &[
                "evald.log.trace",
                ctx.service_name.as_deref().unwrap_or(""),
                n,
                &t_str,
                &rec.observed_time_unix_nano.to_string(),
                &body_digest,
            ],
        ),
    };
    let span_id = derive_id(
        8,
        &[
            "evald.log.span",
            &hex::encode(&trace_id),
            n,
            &t_str,
            &rec.observed_time_unix_nano.to_string(),
            &body_digest,
        ],
    );
    let mut attributes = rec.attributes.clone();
    if let Some(body) = &rec.body {
        attributes.push(KeyValue {
            key: "log.body".into(),
            value: Some(body.clone()),
            ..Default::default()
        });
    }
    if !rec.severity_text.is_empty() {
        attributes.push(kv_str("log.severity_text", &rec.severity_text));
    }
    if rec.severity_number != 0 {
        attributes.push(kv_int("log.severity_number", rec.severity_number as i64));
    }
    attributes.push(kv_str("evald.source", "log"));
    SpanDraft {
        trace_id,
        span_id,
        parent_span_id: parent.map(<[u8]>::to_vec).unwrap_or_default(),
        name: n.to_string(),
        start_unix_nano: t,
        end_unix_nano: t,
        status: None,
        attributes,
    }
    .finish(ctx)
}

pub(crate) fn kv_str(key: &str, val: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AnyVal::StringValue(val.to_string())),
        }),
        ..Default::default()
    }
}

pub(crate) fn kv_int(key: &str, val: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AnyVal::IntValue(val)),
        }),
        ..Default::default()
    }
}

pub(crate) fn kv_f64(key: &str, val: f64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AnyVal::DoubleValue(val)),
        }),
        ..Default::default()
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::ScoreTarget;
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::logs::v1::{ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;

    pub(crate) fn request(
        scope: &str,
        service: &str,
        records: Vec<LogRecord>,
    ) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", service)],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: scope.into(),
                        version: "1.0".into(),
                        ..Default::default()
                    }),
                    log_records: records,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn record(attrs: Vec<KeyValue>) -> LogRecord {
        LogRecord {
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: attrs,
            ..Default::default()
        }
    }

    fn eval_record(trace: Vec<u8>, span: Vec<u8>) -> LogRecord {
        LogRecord {
            event_name: crate::evalevent::EVENT_NAME.into(),
            trace_id: trace,
            span_id: span,
            ..record(vec![
                kv_str("gen_ai.evaluation.name", "relevance"),
                kv_f64("gen_ai.evaluation.score.value", 0.75),
            ])
        }
    }

    #[test]
    fn event_name_prefers_the_attribute_then_the_field_then_a_string_body() {
        let mut r = record(vec![]);
        r.body = Some(AnyValue {
            value: Some(AnyVal::StringValue("from_body".into())),
        });
        r.event_name = "from_field".into();
        assert_eq!(event_name(&r).as_deref(), Some("from_field"));
        // Codex: the field holds a `tracing` callsite, the attribute the event's name.
        r.event_name = "event otel/src/events/session_telemetry.rs:1132".into();
        r.attributes.push(kv_str("event.name", "codex.sse_event"));
        assert_eq!(event_name(&r).as_deref(), Some("codex.sse_event"));
        let mut body_only = record(vec![]);
        body_only.body = r.body.clone();
        assert_eq!(event_name(&body_only).as_deref(), Some("from_body"));
        let mut kv_body = record(vec![]);
        kv_body.body = Some(AnyValue {
            value: Some(AnyVal::IntValue(3)),
        });
        assert_eq!(
            event_name(&kv_body),
            None,
            "a non-string body is not a name"
        );
    }

    #[test]
    fn an_evaluation_result_record_becomes_a_score_on_the_span_it_names() {
        let req = request(
            "my.evals",
            "app",
            vec![eval_record(vec![7; 16], vec![9; 8])],
        );
        let b = normalize_logs(&req, LogsPolicy::Genai);
        assert!(b.spans.is_empty());
        assert_eq!(b.scores.len(), 1);
        let s = &b.scores[0];
        assert_eq!(s.name, "relevance");
        assert_eq!(s.num_value, Some(0.75));
        assert_eq!(s.target, ScoreTarget::Span(hex::encode([9u8; 8])));
        assert_eq!(b.outcomes[Outcome::Score.index()], 1);
    }

    #[test]
    fn an_evaluation_result_without_a_span_targets_its_trace_and_without_either_is_malformed() {
        let req = request(
            "my.evals",
            "app",
            vec![
                eval_record(vec![7; 16], vec![]),
                eval_record(vec![], vec![]),
                // Present but all zero means absent in OTLP.
                eval_record(vec![0; 16], vec![0; 8]),
            ],
        );
        let b = normalize_logs(&req, LogsPolicy::Genai);
        assert_eq!(b.scores.len(), 1);
        assert_eq!(
            b.scores[0].target,
            ScoreTarget::Trace(hex::encode([7u8; 16]))
        );
        assert_eq!(b.outcomes[Outcome::Malformed.index()], 2);
    }

    #[test]
    fn unrecognised_records_are_dropped_by_default_and_kept_under_all() {
        let mut r = record(vec![kv_str("http.route", "/x")]);
        r.body = Some(AnyValue {
            value: Some(AnyVal::StringValue("user logged in".into())),
        });
        r.severity_text = "INFO".into();
        r.severity_number = 9;
        r.trace_id = vec![1; 16];
        r.span_id = vec![2; 8];
        let req = request("app.logger", "app", vec![r]);

        let b = normalize_logs(&req, LogsPolicy::Genai);
        assert!(b.spans.is_empty() && b.scores.is_empty());
        assert_eq!(b.outcomes[Outcome::Unmapped.index()], 1);

        let b = normalize_logs(&req, LogsPolicy::All);
        assert_eq!(b.spans.len(), 1);
        let s = &b.spans[0];
        assert_eq!(s.name, "user logged in", "a string body names the span");
        assert_eq!(s.start_unix_nano, s.end_unix_nano, "zero duration");
        assert_eq!(s.trace_id, hex::encode([1u8; 16]), "the record's own trace");
        assert_eq!(
            s.parent_span_id.as_deref(),
            Some(hex::encode([2u8; 8]).as_str())
        );
        assert_eq!(s.service_name.as_deref(), Some("app"));
        assert_eq!(s.raw_attributes["log.severity_text"], "INFO");
        assert_eq!(s.raw_attributes["log.severity_number"], 9);
        assert_eq!(s.raw_attributes["http.route"], "/x");
        assert_eq!(s.raw_attributes["evald.source"], "log");
        assert_eq!(b.outcomes[Outcome::Span.index()], 1);

        // The same record twice (an exporter retry) lands on the same span id.
        let again = normalize_logs(&req, LogsPolicy::All);
        assert_eq!(again.spans[0].span_id, s.span_id);
    }

    /// Plain log lines carry no trace context. Each is a trace of its own, the same one on a
    /// retry: one trace per service would grow without bound.
    #[test]
    fn untraced_records_kept_under_all_are_a_trace_each() {
        let line = |body: &str, t: u64| {
            let mut r = record(vec![]);
            r.time_unix_nano = t;
            r.body = Some(AnyValue {
                value: Some(AnyVal::StringValue(body.into())),
            });
            r
        };
        let req = request(
            "app.logger",
            "app",
            vec![line("started", 1), line("listening", 2)],
        );
        let b = normalize_logs(&req, LogsPolicy::All);
        assert_eq!(b.spans.len(), 2);
        assert_ne!(b.spans[0].trace_id, b.spans[1].trace_id);
        let again = normalize_logs(&req, LogsPolicy::All);
        assert_eq!(
            again.spans[0].trace_id, b.spans[0].trace_id,
            "stable on a retry"
        );
    }

    #[test]
    fn off_keeps_nothing_not_even_evaluation_results() {
        let req = request(
            "my.evals",
            "app",
            vec![eval_record(vec![7; 16], vec![9; 8])],
        );
        let b = normalize_logs(&req, LogsPolicy::Off);
        assert!(b.spans.is_empty() && b.scores.is_empty());
        assert_eq!(b.records, 1);
        assert_eq!(b.outcomes[Outcome::Off.index()], 1);
    }

    #[test]
    fn outcomes_sum_to_the_records_received() {
        let req = request(
            "mixed",
            "app",
            vec![
                eval_record(vec![7; 16], vec![9; 8]),
                eval_record(vec![], vec![]),
                record(vec![]),
            ],
        );
        for policy in [LogsPolicy::Genai, LogsPolicy::All, LogsPolicy::Off] {
            let b = normalize_logs(&req, policy);
            assert_eq!(b.outcomes.iter().sum::<u64>(), b.records as u64, "{policy}");
        }
    }

    #[test]
    fn the_policy_parses_and_prints() {
        for p in [LogsPolicy::Off, LogsPolicy::Genai, LogsPolicy::All] {
            assert_eq!(p.to_string().parse::<LogsPolicy>(), Ok(p));
        }
        assert_eq!(" ALL ".parse::<LogsPolicy>(), Ok(LogsPolicy::All));
        assert!("verbose".parse::<LogsPolicy>().is_err());
        assert_eq!(LogsPolicy::default(), LogsPolicy::Genai);
    }
}
