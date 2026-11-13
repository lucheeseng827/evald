//! Online sampled evals: `evald serve --online-eval spec.yaml` judges a sample of the live
//! model calls it ingests, with the same judges `eval run` uses, and writes ordinary scores on
//! the spans.
//!
//! The rules come from the cost of getting it wrong (SCOPE-0.3.0 W4/D5):
//! - **Ingest never waits on a judge.** The ingest path only picks spans (a hash and a few
//!   comparisons per model call) and, once its batch is durable, offers them to a bounded queue
//!   with `try_send`. A full queue sheds the span and counts it; a slow or dead judge shows up
//!   as metrics, never as ingest latency or an ingest error.
//! - **Off by default, explicit endpoint.** Nothing runs without a spec, and each judge must
//!   name one of the spec's `endpoints:` (no implicit provider, no implicit localhost).
//! - **One call in flight per endpoint by default** (the endpoint's `max_in_flight`), on a
//!   worker thread of its own, so a judge on the GPU that serves production competes with it as
//!   little as configured.
//! - **A hard daily token cap.** Once the judge calls of the UTC day have spent it, no new call
//!   starts until the next day.
//! - **Errored and costly calls are always judged** (100 %, `always:`); the rest at
//!   `sample_rate`, decided by a hash of the span id so a span gets the same answer every time.
//! - **Co-location guard.** When a model call evald ingests went to the judge's own host
//!   (`server.address` / `server.port`), the judge shares production's server: that is logged
//!   once and shown as a gauge, and with `pause:` judging stops while those calls' recent p95
//!   latency is over the threshold.
//!
//! Scores are advisory: nothing gates on them. The judge's own calls are stored as
//! `evald.judge` spans, like an eval run's, and are never sampled themselves.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::endpoint::{Endpoint, EndpointSpec};
use crate::judge::{JudgeBackend, JudgeCache, JudgeSpec};
use crate::model::NormalizedSpan;
use crate::{DataType, Score, ScoreSource, ScoreTarget, Store};

/// The `config_id` of every online score, so they are one query away (`WHERE config_id =
/// 'online'`) and never mistaken for an eval run's.
pub const ONLINE_CONFIG_ID: &str = "online";

/// `--online-eval spec.yaml`. Unknown keys are an error, as in an eval config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnlineSpec {
    /// Where the judges' calls go; every judge names one of these.
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
    pub judges: Vec<JudgeSpec>,
    /// Share of eligible model calls judged, in `[0, 1]`.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    /// Calls judged whatever the sample rate.
    #[serde(default)]
    pub always: Always,
    /// Only model calls from these services / with these models (empty: any).
    #[serde(default)]
    pub only: Only,
    /// Judge tokens (input + output) one UTC day may spend. Required: an online judge without a
    /// stated ceiling is a bill without one.
    pub daily_token_cap: u64,
    /// Spans waiting for the judge before new ones are shed.
    #[serde(default = "default_queue")]
    pub queue: usize,
    /// Stop judging while production calls on the judge's own host are slow.
    #[serde(default)]
    pub pause: Option<Pause>,
}

fn default_sample_rate() -> f64 {
    0.01
}

fn default_queue() -> usize {
    256
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Always {
    /// Every call whose span status is an error.
    #[serde(default = "yes")]
    pub errors: bool,
    /// Every call that cost at least this much.
    #[serde(default)]
    pub cost_usd_over: Option<f64>,
}

fn yes() -> bool {
    true
}

impl Default for Always {
    fn default() -> Always {
        Always {
            errors: true,
            cost_usd_over: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Only {
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    /// Pause while the p95 duration of recent model calls to the judge's host is above this.
    pub p95_ms_over: f64,
    /// How many recent calls the p95 is over.
    #[serde(default = "default_window")]
    pub window: usize,
}

fn default_window() -> usize {
    200
}

/// The fewest recent calls a p95 is trusted over; below it, judging is not paused.
const PAUSE_MIN_SAMPLES: usize = 20;

impl OnlineSpec {
    pub fn load(path: &Path) -> anyhow::Result<OnlineSpec> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading online-eval spec {}: {e}", path.display()))?;
        serde_yaml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("online-eval spec {}: {e}", path.display()))
    }

    /// Check the spec and resolve each judge's endpoint (`base_dir` for relative paths).
    pub fn resolve(&self, base_dir: &Path) -> anyhow::Result<Vec<(JudgeSpec, Endpoint)>> {
        anyhow::ensure!(!self.judges.is_empty(), "online-eval: `judges:` is empty");
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.sample_rate),
            "online-eval: sample_rate must be between 0 and 1 (got {})",
            self.sample_rate
        );
        anyhow::ensure!(
            self.daily_token_cap > 0,
            "online-eval: daily_token_cap must be > 0"
        );
        anyhow::ensure!(self.queue > 0, "online-eval: queue must be > 0");
        if let Some(p) = &self.pause {
            anyhow::ensure!(
                p.p95_ms_over > 0.0 && p.window >= PAUSE_MIN_SAMPLES,
                "online-eval: pause needs p95_ms_over > 0 and a window of at least \
                 {PAUSE_MIN_SAMPLES}"
            );
        }
        let endpoints = crate::endpoint::resolve_all(&self.endpoints, base_dir)?;
        self.judges
            .iter()
            .map(|j| {
                j.validate()?;
                anyhow::ensure!(
                    j.provider.is_none() && j.endpoint.is_some(),
                    "online-eval: judge {:?} must name one of the spec's `endpoints:` \
                     (`endpoint:`); an online judge has no implicit provider",
                    j.name()
                );
                Ok((j.clone(), j.resolve_endpoint(&endpoints)?))
            })
            .collect()
    }
}

/// Why a span was sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Error,
    Cost,
    Rate,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::Error => "error",
            Why::Cost => "cost",
            Why::Rate => "rate",
        }
    }
}

/// What happened to one (span, judge) pair the worker took from the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Scored,
    /// The call failed (the endpoint is down, refused, timed out).
    Failed,
    /// The rail needs a field the span does not carry.
    Unscorable,
    /// The day's token cap was spent.
    Budget,
    /// Production calls on the judge's host were slow.
    Paused,
}

const OUTCOMES: [Outcome; 5] = [
    Outcome::Scored,
    Outcome::Failed,
    Outcome::Unscorable,
    Outcome::Budget,
    Outcome::Paused,
];

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Scored => "scored",
            Outcome::Failed => "failed",
            Outcome::Unscorable => "unscorable",
            Outcome::Budget => "budget",
            Outcome::Paused => "paused",
        }
    }
}

/// The counters `/metrics` shows.
#[derive(Default)]
pub struct OnlineStats {
    considered: AtomicU64,
    sampled: [AtomicU64; 3],
    shed: AtomicU64,
    queued: AtomicU64,
    judged: [AtomicU64; 5],
    tokens: AtomicU64,
}

/// Tokens spent on the current UTC day. The day is passed in, so tests choose it.
#[derive(Default)]
struct Budget {
    day: AtomicU64,
    used: AtomicU64,
}

impl Budget {
    fn used(&self, day: u64) -> u64 {
        if self.day.swap(day, Ordering::SeqCst) != day {
            self.used.store(0, Ordering::SeqCst);
        }
        self.used.load(Ordering::SeqCst)
    }

    fn spend(&self, day: u64, tokens: u64) {
        self.used(day);
        self.used.fetch_add(tokens, Ordering::SeqCst);
    }
}

fn utc_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400)
}

/// The ingest side and the worker's shared state. [`Store`] holds it when online evals are on.
pub struct Online {
    judges: Vec<(JudgeSpec, Endpoint)>,
    sample_rate: f64,
    always: Always,
    only: Only,
    cap: u64,
    pause: Option<Pause>,
    /// `host:port` (and bare `host`) of each judge endpoint, for the co-location guard.
    judge_hosts: Vec<String>,
    tx: tokio::sync::mpsc::Sender<NormalizedSpan>,
    stats: OnlineStats,
    budget: Budget,
    colocated: AtomicBool,
    /// Recent durations (ms) of model calls that went to a judge's host.
    recent: Mutex<VecDeque<f64>>,
}

/// The worker's end of the queue.
pub struct Queue(tokio::sync::mpsc::Receiver<NormalizedSpan>);

impl Online {
    pub fn new(spec: &OnlineSpec, base_dir: &Path) -> anyhow::Result<(Arc<Online>, Queue)> {
        let judges = spec.resolve(base_dir)?;
        let judge_hosts = judges
            .iter()
            .flat_map(|(_, ep)| host_keys(&ep.base_url))
            .collect();
        let (tx, rx) = tokio::sync::mpsc::channel(spec.queue);
        let online = Online {
            judges,
            sample_rate: spec.sample_rate,
            always: spec.always.clone(),
            only: spec.only.clone(),
            cap: spec.daily_token_cap,
            pause: spec.pause.clone(),
            judge_hosts,
            tx,
            stats: OnlineStats::default(),
            budget: Budget::default(),
            colocated: AtomicBool::new(false),
            recent: Mutex::new(VecDeque::new()),
        };
        Ok((Arc::new(online), Queue(rx)))
    }

    /// Ingest side, before the batch is appended: the spans to judge, cloned. Cheap for the
    /// spans it skips (most of them): no allocation, a hash at most.
    pub fn pick(&self, spans: &[NormalizedSpan]) -> Vec<NormalizedSpan> {
        let mut picked = Vec::new();
        for s in spans {
            if s.service_name.as_deref() == Some(crate::judge::JUDGE_SERVICE) || !s.is_llm_span() {
                continue;
            }
            self.observe_colocation(s);
            if !self.only.services.is_empty()
                && !s
                    .service_name
                    .as_ref()
                    .is_some_and(|n| self.only.services.contains(n))
            {
                continue;
            }
            if !self.only.models.is_empty()
                && !s
                    .model
                    .as_ref()
                    .is_some_and(|m| self.only.models.contains(m))
            {
                continue;
            }
            if s.output_value.as_deref().is_none_or(str::is_empty) {
                continue; // nothing to judge
            }
            self.stats.considered.fetch_add(1, Ordering::Relaxed);
            if let Some(why) = self.wanted(s) {
                self.stats.sampled[why as usize].fetch_add(1, Ordering::Relaxed);
                picked.push(s.clone());
            }
        }
        picked
    }

    /// Whether to judge `s`, and why.
    pub fn wanted(&self, s: &NormalizedSpan) -> Option<Why> {
        if self.always.errors && s.status_code == 2 {
            return Some(Why::Error);
        }
        if let (Some(over), Some(cost)) = (self.always.cost_usd_over, s.cost_usd) {
            if cost >= over {
                return Some(Why::Cost);
            }
        }
        (unit_hash(&s.trace_id, &s.span_id) < self.sample_rate).then_some(Why::Rate)
    }

    /// Ingest side, once the batch is durable: queue the picked spans, shedding what does not
    /// fit. Never waits.
    pub fn offer(&self, picked: Vec<NormalizedSpan>) {
        for s in picked {
            match self.tx.try_send(s) {
                Ok(()) => {
                    self.stats.queued.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.stats.shed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn observe_colocation(&self, s: &NormalizedSpan) {
        let Some(host) = s
            .raw_attributes
            .get("server.address")
            .and_then(|v| v.as_str())
        else {
            return;
        };
        let key = match s.raw_attributes.get("server.port").and_then(|v| v.as_i64()) {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        if !self.judge_hosts.contains(&key) {
            return;
        }
        if !self.colocated.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                server = %key,
                "online-eval: production model calls go to the judge's own server; the judge \
                 competes with them (use a separate endpoint, or set `pause:`)"
            );
        }
        if let Some(p) = &self.pause {
            let ms = s.end_unix_nano.saturating_sub(s.start_unix_nano) as f64 / 1e6;
            let mut recent = self.recent.lock().expect("online latency window");
            recent.push_back(ms);
            while recent.len() > p.window {
                recent.pop_front();
            }
        }
    }

    /// The p95 of recent production calls on the judge's host is over the threshold.
    pub fn paused(&self) -> bool {
        let Some(p) = &self.pause else { return false };
        let recent = self.recent.lock().expect("online latency window");
        if recent.len() < PAUSE_MIN_SAMPLES {
            return false;
        }
        let mut v: Vec<f64> = recent.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        let rank = ((0.95 * v.len() as f64).ceil() as usize).clamp(1, v.len());
        v[rank - 1] > p.p95_ms_over
    }

    fn count(&self, o: Outcome) {
        self.stats.judged[o as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Queued spans not yet taken by the worker.
    pub fn queue_depth(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }

    /// The `/metrics` lines.
    pub fn render_metrics(&self, out: &mut String) {
        use crate::metrics::{counter, counter_labeled, gauge};
        let st = &self.stats;
        counter(
            out,
            "evald_online_eval_considered_total",
            "Model calls with an output that passed the online-eval filter.",
            st.considered.load(Ordering::Relaxed) as f64,
        );
        let sampled: Vec<(String, u64)> = [Why::Error, Why::Cost, Why::Rate]
            .iter()
            .map(|&w| {
                (
                    w.as_str().to_string(),
                    st.sampled[w as usize].load(Ordering::Relaxed),
                )
            })
            .collect();
        counter_labeled(
            out,
            "evald_online_eval_sampled_total",
            "Model calls picked for judging, by why (error, cost, rate).",
            "why",
            &sampled,
        );
        counter(
            out,
            "evald_online_eval_shed_total",
            "Picked calls dropped because the judge queue was full (ingest never waits).",
            st.shed.load(Ordering::Relaxed) as f64,
        );
        let judged: Vec<(String, u64)> = OUTCOMES
            .iter()
            .map(|&o| {
                (
                    o.as_str().to_string(),
                    st.judged[o as usize].load(Ordering::Relaxed),
                )
            })
            .collect();
        counter_labeled(
            out,
            "evald_online_eval_judged_total",
            "(call, judge) pairs the worker handled, by outcome.",
            "outcome",
            &judged,
        );
        counter(
            out,
            "evald_online_eval_tokens_total",
            "Tokens (input + output) the online judge calls spent.",
            st.tokens.load(Ordering::Relaxed) as f64,
        );
        gauge(
            out,
            "evald_online_eval_tokens_today",
            "Tokens spent so far this UTC day, against evald_online_eval_daily_token_cap.",
            self.budget.used(utc_day()) as f64,
        );
        gauge(
            out,
            "evald_online_eval_daily_token_cap",
            "The spec's daily_token_cap.",
            self.cap as f64,
        );
        gauge(
            out,
            "evald_online_eval_queue_depth",
            "Picked calls waiting for the judge.",
            self.queue_depth() as f64,
        );
        gauge(
            out,
            "evald_online_eval_colocated",
            "1 once a production model call went to a judge endpoint's own server.",
            u8::from(self.colocated.load(Ordering::Relaxed)) as f64,
        );
        gauge(
            out,
            "evald_online_eval_paused",
            "1 while judging is paused for slow production calls on the judge's server.",
            u8::from(self.paused()) as f64,
        );
    }
}

/// `host:port` and `host` of a base URL (`http://gpu-box:8000` → both), for matching a span's
/// `server.address` / `server.port`. The scheme's default port is filled in.
fn host_keys(base_url: &str) -> Vec<String> {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("http", base_url));
    let authority = rest.split('/').next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    // `[::1]:8000`: the brackets belong to the URL, not to `server.address`.
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let (h, after) = inner.split_once(']').unwrap_or((inner, ""));
            (h, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    let port = port.unwrap_or(if scheme == "https" { "443" } else { "80" });
    vec![format!("{host}:{port}"), host.to_string()]
}

/// A span's place in `[0, 1)`, from its ids: the sampling decision is the same every time the
/// span is seen.
fn unit_hash(trace_id: &str, span_id: &str) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in trace_id.bytes().chain(*b"/").chain(span_id.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // FNV's low bits are weak for short keys; mix before scaling.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// The text a span field holds, resolving an offloaded payload.
fn payload(store: &Store, v: &Option<String>) -> Option<String> {
    let v = v.as_deref()?;
    if v.starts_with(crate::blob::BLOB_REF_PREFIX) {
        let bytes = store.get_blob(v).ok()??;
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    Some(v.to_string())
}

/// Judge one picked span with every judge, and hand what came back to the writer.
#[allow(clippy::too_many_arguments)]
async fn judge_span<B: JudgeBackend>(
    online: &Online,
    store: &Store,
    writes: &tokio::sync::mpsc::Sender<Write>,
    backend: &B,
    cache: &JudgeCache,
    provenances: &[Option<crate::judge::Provenance>],
    permits: &BTreeMap<String, tokio::sync::Semaphore>,
    span: NormalizedSpan,
) {
    let Some(output) = payload(store, &span.output_value) else {
        return;
    };
    let item = crate::eval::DatasetItem {
        id: None,
        input: payload(store, &span.input_value),
        output,
        expected_output: None,
        span_id: Some(span.span_id.clone()),
        trace_id: Some(span.trace_id.clone()),
        context: None,
        metadata: None,
    };
    for ((spec, ep), provenance) in online.judges.iter().zip(provenances) {
        let identity = provenance.as_ref().and_then(crate::judge::Provenance::key);
        let day = utc_day();
        if online.budget.used(day) >= online.cap {
            online.count(Outcome::Budget);
            continue;
        }
        if online.paused() {
            online.count(Outcome::Paused);
            continue;
        }
        let _permit = permits[&ep.name].acquire().await.expect("never closed");
        let result =
            crate::judge::score_one(spec, ep, &item, backend, cache, identity.as_deref()).await;
        // What went over the network for this call, whatever came of it.
        let egress = crate::egress::spans(&backend.drain_egress(), &format!("online-{day}"));
        let scored = match result {
            Ok(s) => s,
            Err(e) => {
                online.count(Outcome::Failed);
                tracing::warn!(judge = %spec.name(), error = %format!("{e:#}"), "online-eval: judge call failed");
                if !egress.is_empty() {
                    let _ = writes
                        .send(Write {
                            spans: egress,
                            score: None,
                        })
                        .await;
                }
                continue;
            }
        };
        let Some(outcome) = scored.outcome else {
            online.count(Outcome::Unscorable);
            continue;
        };
        let mut spans = egress;
        if let Some(call) = scored.call {
            let tokens = call.usage.map_or_else(
                || crate::judge::estimate_tokens(&item, spec),
                |(i, o)| i + o,
            );
            online.budget.spend(day, tokens);
            online.stats.tokens.fetch_add(tokens, Ordering::Relaxed);
            let summary = crate::judge::JudgeSummary {
                name: spec.name(),
                rail: spec.rail,
                model: spec.model.clone(),
                endpoint: ep.clone(),
                identity: identity.clone(),
                provenance: provenance.clone(),
                calls: vec![call],
                cached: 0,
            };
            // One trace per UTC day; each call's id from the span it judged (the call index
            // `call_spans` uses is always 0 here), with that span named on it.
            let mut calls = crate::judge::call_spans(&format!("online-{day}"), &[summary]);
            for s in &mut calls {
                s.span_id = hex::encode(crate::logs::derive_id(
                    8,
                    &["evald.judge.online", &spec.name(), &span.span_id],
                ));
                s.raw_attributes.insert(
                    "evald.judge.target_span_id".into(),
                    span.span_id.clone().into(),
                );
            }
            spans.extend(calls);
        }
        let score = Score {
            id: format!("online:{}:{}", spec.name(), span.span_id),
            target: ScoreTarget::Span(span.span_id.clone()),
            name: spec.name(),
            num_value: Some(outcome.value),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: outcome.comment,
            config_id: Some(ONLINE_CONFIG_ID.to_string()),
            agg_stats: None,
            ts_unix_nano: crate::judge::unix_nanos_now(),
        };
        // Counted `Scored` (or `Failed`) by the writer, once the write's fate is known.
        let _ = writes
            .send(Write {
                spans,
                score: Some(score),
            })
            .await;
    }
}

/// Judge results waiting for one durable write: a score with its judge-call spans, or (from a
/// failed call) only the egress spans.
struct Write {
    spans: Vec<NormalizedSpan>,
    score: Option<Score>,
}

/// Results written together at most; also the writer's channel depth, in batches of this.
const WRITE_BATCH: usize = 64;

/// Write judge results as they arrive, everything waiting at that moment in one batch. A lone
/// result is written at once, as before; when results come faster than a write takes (a judge
/// that answers from its cache, a fast local model), they share one WAL append and one score
/// commit instead of paying an fsync each, which is what competed with ingest. A full channel
/// holds the judge back, so the ingest-side queue still sheds rather than this growing.
async fn write_results(online: &Online, store: &Store, mut rx: tokio::sync::mpsc::Receiver<Write>) {
    while let Some(first) = rx.recv().await {
        let mut spans = first.spans;
        let mut scores: Vec<Score> = first.score.into_iter().collect();
        while scores.len() < WRITE_BATCH {
            let Ok(next) = rx.try_recv() else { break };
            spans.extend(next.spans);
            scores.extend(next.score);
        }
        match store.ingest_batch(spans, &scores).await {
            Ok(_) => scores.iter().for_each(|_| online.count(Outcome::Scored)),
            Err(e) => {
                scores.iter().for_each(|_| online.count(Outcome::Failed));
                tracing::warn!(error = %e, scores = scores.len(), "online-eval: storing judge scores failed");
            }
        }
    }
}

/// Run the worker on its own thread and runtime, so a judge (and whatever its backend's
/// futures hold) never runs on the ingest runtime. Returns once the thread is started.
pub fn start<B: JudgeBackend + Send + 'static>(
    online: Arc<Online>,
    queue: Queue,
    store: Store,
    backend: B,
    cache: JudgeCache,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("evald-online-eval".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "online-eval: could not start its runtime");
                    return;
                }
            };
            rt.block_on(run(online, queue, store, backend, cache));
        })
}

/// The worker loop: until the queue closes (the store, and every sender, dropped).
pub async fn run<B: JudgeBackend>(
    online: Arc<Online>,
    queue: Queue,
    store: Store,
    backend: B,
    cache: JudgeCache,
) {
    use futures::StreamExt;
    let mut provenances = Vec::with_capacity(online.judges.len());
    for (spec, ep) in &online.judges {
        provenances.push(match backend.provenance(ep, &spec.model).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(judge = %spec.name(), error = %format!("{e:#}"), "online-eval: could not read the judge endpoint's model list");
                None
            }
        });
    }
    let permits: BTreeMap<String, tokio::sync::Semaphore> = online
        .judges
        .iter()
        .map(|(_, ep)| {
            (
                ep.name.clone(),
                tokio::sync::Semaphore::new(ep.max_in_flight),
            )
        })
        .collect();
    let width = online
        .judges
        .iter()
        .map(|(_, ep)| (ep.name.as_str(), ep.max_in_flight))
        .collect::<BTreeMap<_, _>>()
        .values()
        .sum::<usize>()
        .max(1);
    let stream =
        futures::stream::unfold(
            queue.0,
            |mut rx| async move { rx.recv().await.map(|s| (s, rx)) },
        );
    let (tx, rx) = tokio::sync::mpsc::channel(WRITE_BATCH * 4);
    let (online, store, backend, cache, provenances, permits) =
        (&online, &store, &backend, &cache, &provenances, &permits);
    let judging = async move {
        let writes = &tx;
        stream
            .for_each_concurrent(width, |span| async move {
                judge_span(
                    online,
                    store,
                    writes,
                    backend,
                    cache,
                    provenances,
                    permits,
                    span,
                )
                .await;
            })
            .await;
        // `tx` dropped here: the writer finishes what is queued, then stops.
    };
    futures::join!(judging, write_results(online, store, rx));
}

/// `serve --online-eval`: load the spec, attach the sampler to the store, start the worker.
#[cfg(feature = "judge")]
pub fn attach(store: &Store, spec_path: &Path, data_dir: &Path) -> anyhow::Result<()> {
    let spec = OnlineSpec::load(spec_path)?;
    let base = spec_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (online, queue) = Online::new(&spec, base)?;
    let cache = JudgeCache::open(data_dir)
        .map_err(|e| anyhow::anyhow!("opening judge cache in {}: {e}", data_dir.display()))?;
    for (spec, ep) in &online.judges {
        tracing::info!(
            judge = %spec.name(),
            endpoint = %ep.name,
            base_url = %ep.base_url,
            max_in_flight = ep.max_in_flight,
            "online-eval: judging sampled model calls"
        );
    }
    store.set_online(online.clone());
    start(
        online,
        queue,
        store.clone(),
        crate::judge::http_backend(),
        cache,
    )?;
    Ok(())
}

/// Without the judge feature there is no backend to judge with: refuse at startup, by name.
#[cfg(not(feature = "judge"))]
pub fn attach(_store: &Store, _spec_path: &Path, _data_dir: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "--online-eval needs the judge backend; this build has none — rebuild with \
         `--features judge`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{JudgeCall, JudgeReply};
    use crate::StoreConfig;
    use std::sync::atomic::AtomicUsize;

    fn spec(yaml: &str) -> OnlineSpec {
        serde_yaml::from_str(yaml).unwrap()
    }

    const BASE: &str = "endpoints:\n  - name: box\n    api: openai-chat\n    base_url: \
                        http://judge-box:8000\njudges:\n  - rail: toxicity\n    endpoint: box\n    \
                        model: m\ndaily_token_cap: 1000\n";

    fn online(extra: &str) -> (Arc<Online>, Queue) {
        Online::new(&spec(&format!("{BASE}{extra}")), Path::new(".")).unwrap()
    }

    fn llm(span_id: &str) -> NormalizedSpan {
        let mut s = crate::store::test_span("t1", span_id, 1_000_000);
        s.oi_kind = Some("LLM".into());
        s.model = Some("prod-model".into());
        s.service_name = Some("app".into());
        s.input_value = Some("question".into());
        s.output_value = Some("answer".into());
        s.start_unix_nano = 1_000_000;
        s.end_unix_nano = 2_000_000;
        s
    }

    #[test]
    fn a_spec_needs_an_explicit_endpoint_and_a_cap() {
        assert!(Online::new(&spec(BASE), Path::new(".")).is_ok());
        let hosted = "judges:\n  - rail: toxicity\n    provider: openai\n    model: m\n\
                      daily_token_cap: 10\n";
        let err = Online::new(&spec(hosted), Path::new("."))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no implicit provider"), "{err}");
        let no_cap: Result<OnlineSpec, _> =
            serde_yaml::from_str(&BASE.replace("daily_token_cap: 1000\n", ""));
        assert!(no_cap.unwrap_err().to_string().contains("daily_token_cap"));
        let bad: Result<OnlineSpec, _> = serde_yaml::from_str(&format!("{BASE}rate: 0.5\n"));
        assert!(bad.is_err(), "unknown keys are refused");
        let err = Online::new(&spec(&format!("{BASE}sample_rate: 2\n")), Path::new("."))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("sample_rate"), "{err}");
    }

    #[test]
    fn the_example_spec_loads() {
        let s: OnlineSpec =
            serde_yaml::from_str(include_str!("../examples/eval/online.yaml")).unwrap();
        let (o, _q) = Online::new(&s, Path::new(".")).unwrap();
        assert_eq!(o.judges[0].1.name, "judge-box");
        assert_eq!(o.cap, 2_000_000);
    }

    #[test]
    fn errors_and_costly_calls_are_always_picked_the_rest_by_rate() {
        let (o, _q) = online("sample_rate: 0\nalways:\n  cost_usd_over: 0.5\n");
        let mut err = llm("e");
        err.status_code = 2;
        let mut dear = llm("d");
        dear.cost_usd = Some(0.75);
        let plain = llm("p");
        assert_eq!(o.wanted(&err), Some(Why::Error));
        assert_eq!(o.wanted(&dear), Some(Why::Cost));
        assert_eq!(o.wanted(&plain), None);
        let (all, _q) = online("sample_rate: 1\n");
        assert_eq!(all.wanted(&plain), Some(Why::Rate));

        // Not model calls, the judge's own calls, a call with no output, or outside `only:`:
        // never considered.
        let (o, _q) = online("sample_rate: 1\nonly:\n  services: [app]\n");
        let mut tool = llm("tool");
        tool.oi_kind = Some("TOOL".into());
        let mut judge = llm("j");
        judge.service_name = Some(crate::judge::JUDGE_SERVICE.into());
        let mut empty = llm("x");
        empty.output_value = None;
        let mut other = llm("o");
        other.service_name = Some("batch".into());
        let picked = o.pick(&[tool, judge, empty, other, llm("ok")]);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].span_id, "ok");
        assert_eq!(o.stats.considered.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_sample_rate_is_honoured_and_the_same_span_gets_the_same_answer() {
        let (o, _q) = online("sample_rate: 0.05\n");
        let picked = (0..20_000)
            .filter(|i| o.wanted(&llm(&format!("{i:016x}"))).is_some())
            .count();
        assert!((800..1200).contains(&picked), "{picked} of 20000 at 5%");
        let s = llm("0123456789abcdef");
        assert_eq!(o.wanted(&s), o.wanted(&s));
    }

    #[test]
    fn a_full_queue_sheds_and_counts_and_never_waits() {
        let (o, _q) = online("sample_rate: 1\nqueue: 2\n");
        let picked = o.pick(&[llm("a"), llm("b"), llm("c"), llm("d")]);
        let t = std::time::Instant::now();
        o.offer(picked);
        assert!(t.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(o.stats.queued.load(Ordering::Relaxed), 2);
        assert_eq!(o.stats.shed.load(Ordering::Relaxed), 2);
        assert_eq!(o.queue_depth(), 2);
    }

    #[test]
    fn the_budget_resets_each_utc_day() {
        let b = Budget::default();
        b.spend(100, 600);
        b.spend(100, 500);
        assert_eq!(b.used(100), 1100);
        assert_eq!(b.used(101), 0, "a new day starts at zero");
        b.spend(101, 7);
        assert_eq!(b.used(101), 7);
    }

    #[test]
    fn calls_to_the_judges_own_server_flag_colocation_and_can_pause() {
        let (o, _q) = online("pause:\n  p95_ms_over: 500\n  window: 20\n");
        assert!(!o.paused());
        let mut prod = llm("p");
        prod.raw_attributes
            .insert("server.address".into(), "judge-box".into());
        prod.raw_attributes
            .insert("server.port".into(), 8000.into());
        prod.end_unix_nano = prod.start_unix_nano + 900_000_000; // 900 ms
        let elsewhere = {
            let mut s = prod.clone();
            s.raw_attributes
                .insert("server.address".into(), "api.example".into());
            s
        };
        o.pick(&[elsewhere]);
        assert!(!o.colocated.load(Ordering::Relaxed));
        for _ in 0..19 {
            o.pick(std::slice::from_ref(&prod));
        }
        assert!(o.colocated.load(Ordering::Relaxed));
        assert!(!o.paused(), "19 calls are too few to trust a p95");
        o.pick(std::slice::from_ref(&prod));
        assert!(o.paused());
        // Fast calls push the slow ones out of the window.
        let mut fast = prod.clone();
        fast.end_unix_nano = fast.start_unix_nano + 50_000_000;
        for _ in 0..20 {
            o.pick(std::slice::from_ref(&fast));
        }
        assert!(!o.paused());
    }

    #[test]
    fn host_keys_match_server_address_and_port() {
        assert_eq!(
            host_keys("http://gpu-box:8000"),
            ["gpu-box:8000", "gpu-box"]
        );
        assert_eq!(
            host_keys("https://llm.internal/v1"),
            ["llm.internal:443", "llm.internal"]
        );
        assert_eq!(host_keys("http://[::1]:11434"), ["::1:11434", "::1"]);
        assert_eq!(host_keys("http://[::1]/v1"), ["::1:80", "::1"]);
        assert_eq!(host_keys("http://user@box:9000"), ["box:9000", "box"]);
    }

    /// A backend that scores 0.8, fails on demand, and records how many calls overlap.
    #[derive(Default)]
    struct Scripted {
        fail: AtomicBool,
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        most: AtomicUsize,
    }

    impl JudgeBackend for Scripted {
        async fn identify(&self, _: &Endpoint, _: &str) -> anyhow::Result<Option<String>> {
            Ok(Some("weights-v1".into()))
        }
        async fn complete(&self, _: &Endpoint, _: JudgeCall<'_>) -> anyhow::Result<JudgeReply> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "connection refused");
            Ok(JudgeReply {
                usage: Some((300, 20)),
                ..JudgeReply::text("{\"score\": 0.8}")
            })
        }
    }

    impl JudgeBackend for Arc<Scripted> {
        async fn identify(&self, e: &Endpoint, m: &str) -> anyhow::Result<Option<String>> {
            self.as_ref().identify(e, m).await
        }
        async fn complete(&self, e: &Endpoint, c: JudgeCall<'_>) -> anyhow::Result<JudgeReply> {
            self.as_ref().complete(e, c).await
        }
    }

    fn store(dir: &Path) -> Store {
        Store::open(
            dir,
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap()
    }

    async fn until(what: &str, f: impl Fn() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn judged(o: &Online, outcome: Outcome) -> u64 {
        o.stats.judged[outcome as usize].load(Ordering::Relaxed)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingested_calls_are_judged_in_the_background_within_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        // 320 tokens a call: the third call would start at 640 < 700, the fourth is refused.
        let (o, q) = Online::new(
            &spec(&BASE.replace(
                "daily_token_cap: 1000",
                "daily_token_cap: 700\nsample_rate: 1",
            )),
            Path::new("."),
        )
        .unwrap();
        st.set_online(o.clone());
        let backend = Arc::new(Scripted::default());
        start(
            o.clone(),
            q,
            st.clone(),
            backend.clone(),
            JudgeCache::open(dir.path()).unwrap().without_reads(),
        )
        .unwrap();

        let spans: Vec<NormalizedSpan> = (0..5).map(|i| llm(&format!("s{i}"))).collect();
        st.ingest_batch(spans, &[]).await.unwrap();
        until("five judged", || {
            judged(&o, Outcome::Scored) + judged(&o, Outcome::Budget) == 5
        })
        .await;
        assert_eq!(judged(&o, Outcome::Scored), 3);
        assert_eq!(judged(&o, Outcome::Budget), 2, "the cap stops new calls");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            backend.most.load(Ordering::SeqCst),
            1,
            "max_in_flight defaults to 1"
        );

        // Ordinary scores on the spans, marked online...
        let scores = st
            .scores_for_target(&ScoreTarget::Span("s0".into()))
            .unwrap();
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].name, "judge_toxicity");
        assert_eq!(scores[0].num_value, Some(0.8));
        assert_eq!(scores[0].config_id.as_deref(), Some(ONLINE_CONFIG_ID));
        // ...and the judge's own calls as evald.judge spans, which are never sampled again.
        let mut metrics = String::new();
        o.render_metrics(&mut metrics);
        assert!(
            metrics.contains("evald_online_eval_tokens_total 960"),
            "{metrics}"
        );
        assert!(
            metrics.contains("evald_online_eval_considered_total 5"),
            "{metrics}"
        );
        assert!(metrics.contains("evald_online_eval_judged_total{outcome=\"budget\"} 2"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_judge_outage_is_a_metric_not_an_ingest_error() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let (o, q) = online("sample_rate: 1\n");
        st.set_online(o.clone());
        let backend = Arc::new(Scripted::default());
        backend.fail.store(true, Ordering::SeqCst);
        start(
            o.clone(),
            q,
            st.clone(),
            backend.clone(),
            JudgeCache::open(dir.path()).unwrap(),
        )
        .unwrap();
        let n = st
            .ingest_batch(vec![llm("a"), llm("b")], &[])
            .await
            .unwrap();
        assert_eq!(n, 2, "ingest is unaffected");
        until("two failures", || judged(&o, Outcome::Failed) == 2).await;
        assert!(st
            .scores_for_target(&ScoreTarget::Span("a".into()))
            .unwrap()
            .is_empty());
        // The judge comes back: the next calls are scored.
        backend.fail.store(false, Ordering::SeqCst);
        st.ingest_batch(vec![llm("c")], &[]).await.unwrap();
        until("one scored", || judged(&o, Outcome::Scored) == 1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_in_flight_cap_holds_under_load() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let (o, q) = Online::new(
            &spec(
                &BASE
                    .replace(
                        "base_url: http://judge-box:8000",
                        "base_url: http://judge-box:8000\n    max_in_flight: 3",
                    )
                    .replace(
                        "daily_token_cap: 1000",
                        "daily_token_cap: 1000000\nsample_rate: 1",
                    ),
            ),
            Path::new("."),
        )
        .unwrap();
        st.set_online(o.clone());
        let backend = Arc::new(Scripted::default());
        start(
            o.clone(),
            q,
            st.clone(),
            backend.clone(),
            JudgeCache::open(dir.path()).unwrap().without_reads(),
        )
        .unwrap();
        let spans: Vec<NormalizedSpan> = (0..40).map(|i| llm(&format!("s{i}"))).collect();
        st.ingest_batch(spans, &[]).await.unwrap();
        until("forty scored", || judged(&o, Outcome::Scored) == 40).await;
        assert_eq!(backend.most.load(Ordering::SeqCst), 3);
    }
}
