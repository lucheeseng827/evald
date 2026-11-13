//! One door for every outbound call evald makes, and a ledger of what went through it.
//!
//! The default build has no HTTP client at all (`scripts/check-no-network.sh`), so it can make no
//! outbound call. The `judge` build can, and only through the judge backend, which builds every
//! client here first:
//!
//! - **`--offline`**: no client is built, so nothing connects. A judge that would call out fails
//!   with an error that names the flag.
//! - **`--egress-allow <host|IP|CIDR>`** (repeatable): only these. The host is resolved when its
//!   client is built, checked by name or by every address it resolved to, and the client is
//!   pinned to the address that passed, so a DNS change between the check and the connection
//!   cannot send it elsewhere. Redirects are not followed.
//!
//! Every request the backend sends, and every one the door refuses, is recorded: when, which
//! endpoint, host, the address it connected to, bytes each way, status, model, purpose, the run,
//! and the endpoint's `jurisdiction:` label. The records are stored as spans of service
//! `evald.egress` (no new table; format 1 is unchanged), and `evald egress report` reads them.
//! What it reports is what evald recorded, a statement of fact rather than a compliance claim.

use std::net::IpAddr;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::model::NormalizedSpan;
use crate::{Store, StoreConfig};

/// The `service.name` of the ledger's spans.
pub const EGRESS_SERVICE: &str = "evald.egress";

/// One entry of `--egress-allow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allow {
    /// A host name, matched case-insensitively, whole.
    Host(String),
    /// An address and a prefix length: `10.0.0.0/8`, `fd00::/8`, or one address (`/32`, `/128`).
    Net(IpAddr, u8),
}

impl Allow {
    pub fn parse(s: &str) -> anyhow::Result<Allow> {
        let s = s.trim();
        anyhow::ensure!(!s.is_empty(), "an empty --egress-allow entry");
        if let Some((addr, len)) = s.split_once('/') {
            let ip: IpAddr = addr.parse().map_err(|_| {
                anyhow::anyhow!("--egress-allow {s:?}: {addr:?} is not an IP address")
            })?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let len: u8 = len.parse().ok().filter(|l| *l <= max).ok_or_else(|| {
                anyhow::anyhow!("--egress-allow {s:?}: the prefix must be 0-{max}")
            })?;
            return Ok(Allow::Net(ip, len));
        }
        if let Ok(ip) = s.trim_matches(['[', ']']).parse::<IpAddr>() {
            return Ok(Allow::Net(ip, if ip.is_ipv4() { 32 } else { 128 }));
        }
        anyhow::ensure!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
            "--egress-allow {s:?} is not a host name, an IP address or a CIDR"
        );
        Ok(Allow::Host(s.to_ascii_lowercase()))
    }

    fn admits_ip(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Allow::Net(IpAddr::V4(net), len), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(*len)).unwrap_or(0);
                u32::from(*net) & mask == u32::from(ip) & mask
            }
            (Allow::Net(IpAddr::V6(net), len), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(*len)).unwrap_or(0);
                u128::from(*net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// What evald may call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub offline: bool,
    /// Empty: anything (the default). Otherwise only these.
    pub allow: Vec<Allow>,
}

impl Policy {
    pub fn new(offline: bool, allow: &[String]) -> anyhow::Result<Policy> {
        anyhow::ensure!(
            !(offline && !allow.is_empty()),
            "--offline makes no outbound call at all, so --egress-allow would have no effect: \
             give one or the other"
        );
        Ok(Policy {
            offline,
            allow: allow
                .iter()
                .map(|a| Allow::parse(a))
                .collect::<anyhow::Result<_>>()?,
        })
    }

    /// Whether calls are restricted to a list (so hosts must be resolved and checked).
    pub fn restricts(&self) -> bool {
        !self.allow.is_empty()
    }

    /// The address to connect to for `host`, which resolved to `addrs`: the first the list admits
    /// (a host-name entry admits them all). `Err` says why the call is refused.
    pub fn admit(&self, host: &str, addrs: &[IpAddr]) -> Result<Option<IpAddr>, String> {
        if self.offline {
            return Err(format!(
                "--offline: evald makes no outbound call (to {host})"
            ));
        }
        if !self.restricts() {
            return Ok(addrs.first().copied());
        }
        let name = host.trim_matches(['[', ']']).to_ascii_lowercase();
        if self
            .allow
            .iter()
            .any(|a| matches!(a, Allow::Host(h) if *h == name))
        {
            return addrs
                .first()
                .copied()
                .map(Some)
                .ok_or_else(|| format!("{host} did not resolve"));
        }
        addrs
            .iter()
            .copied()
            .find(|ip| self.allow.iter().any(|a| a.admits_ip(*ip)))
            .map(Some)
            .ok_or_else(|| {
                let shown: Vec<String> = addrs.iter().map(IpAddr::to_string).collect();
                format!(
                    "{host} ({}) is not in --egress-allow",
                    if shown.is_empty() {
                        "unresolved".into()
                    } else {
                        shown.join(", ")
                    }
                )
            })
    }

    /// One line for `doctor` and the logs.
    pub fn describe(&self) -> String {
        if self.offline {
            "offline: no outbound call".into()
        } else if self.allow.is_empty() {
            "any host an endpoint names (no --egress-allow)".into()
        } else {
            let list: Vec<String> = self
                .allow
                .iter()
                .map(|a| match a {
                    Allow::Host(h) => h.clone(),
                    Allow::Net(ip, len) => format!("{ip}/{len}"),
                })
                .collect();
            format!("only {}", list.join(", "))
        }
    }
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Set the process's policy, once, from the command line. A second call is ignored.
pub fn configure(policy: Policy) {
    let _ = POLICY.set(policy);
}

/// The process's policy (open when none was configured).
pub fn policy() -> &'static Policy {
    POLICY.get_or_init(Policy::default)
}

/// Why a call was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A judge in `eval run`.
    Judge,
    /// A judge in `serve --online-eval`.
    OnlineJudge,
    /// `evald judge probe` / `doctor --judge-config`.
    Probe,
    /// `evald replay`.
    Replay,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Judge => "judge",
            Purpose::OnlineJudge => "online-judge",
            Purpose::Probe => "probe",
            Purpose::Replay => "replay",
        }
    }
}

/// One outbound request, or one the door refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub endpoint: String,
    pub jurisdiction: Option<String>,
    pub host: String,
    pub port: u16,
    /// The address connected to; `None` when the request never connected.
    pub ip: Option<IpAddr>,
    pub method: &'static str,
    pub path: String,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub status: Option<u16>,
    pub model: Option<String>,
    pub purpose: Purpose,
    /// `ok` (any response), `refused` (by the door), `error` (no response).
    pub outcome: &'static str,
}

/// The records a backend has made and not yet handed over.
#[derive(Debug, Default)]
pub struct Ledger(Mutex<Vec<Record>>);

impl Ledger {
    pub fn record(&self, r: Record) {
        self.0.lock().expect("egress ledger").push(r);
    }

    /// Everything recorded so far, oldest first; the ledger is empty after.
    pub fn drain(&self) -> Vec<Record> {
        std::mem::take(&mut *self.0.lock().expect("egress ledger"))
    }
}

/// The records as spans of service [`EGRESS_SERVICE`], one trace per `run` (a run id, or a
/// day for a long-running server). Never model calls: they carry no `gen_ai.*` key.
pub fn spans(records: &[Record], run: &str) -> Vec<NormalizedSpan> {
    use crate::logs::{derive_id, kv_int, kv_str};
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{
        span::SpanKind, status::StatusCode, ResourceSpans, ScopeSpans, Span, Status,
    };
    if records.is_empty() {
        return Vec::new();
    }
    let trace_id = derive_id(16, &["evald.egress", run]);
    let spans = records
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut attributes = vec![
                kv_str("server.address", &r.host),
                kv_int("server.port", i64::from(r.port)),
                kv_str("http.request.method", r.method),
                kv_str("url.path", &r.path),
                kv_str("evald.run_id", run),
                kv_str("evald.egress.endpoint", &r.endpoint),
                kv_str("evald.egress.purpose", r.purpose.as_str()),
                kv_str("evald.egress.outcome", r.outcome),
                kv_int("evald.egress.bytes_out", r.bytes_out as i64),
                kv_int("evald.egress.bytes_in", r.bytes_in as i64),
            ];
            if let Some(ip) = r.ip {
                attributes.push(kv_str("network.peer.address", &ip.to_string()));
            }
            if let Some(s) = r.status {
                attributes.push(kv_int("http.response.status_code", i64::from(s)));
            }
            if let Some(j) = &r.jurisdiction {
                attributes.push(kv_str("evald.egress.jurisdiction", j));
            }
            if let Some(m) = &r.model {
                attributes.push(kv_str("evald.egress.model", m));
            }
            Span {
                trace_id: trace_id.clone(),
                span_id: derive_id(
                    8,
                    &[
                        "evald.egress.call",
                        run,
                        &r.start_unix_nano.to_string(),
                        &i.to_string(),
                    ],
                ),
                name: format!("{} {}", r.method, r.host),
                kind: SpanKind::Client as i32,
                start_time_unix_nano: r.start_unix_nano,
                end_time_unix_nano: r.end_unix_nano.max(r.start_unix_nano),
                attributes,
                status: (r.outcome != "ok").then(|| Status {
                    code: StatusCode::Error as i32,
                    message: r.outcome.into(),
                }),
                ..Default::default()
            }
        })
        .collect();
    crate::normalize::normalize_request(&ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv_str("service.name", EGRESS_SERVICE)],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: EGRESS_SERVICE.into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                }),
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    })
}

/// One row of `evald egress report`: a destination and what went to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub host: String,
    pub ip: String,
    pub jurisdiction: String,
    pub purpose: String,
    pub calls: u64,
    pub refused: u64,
    pub errors: u64,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub first_unix_nano: u64,
    pub last_unix_nano: u64,
}

/// A number out of an attribute in `raw_attributes_json`, or the attribute's text, as SQL.
fn attr_sql(key: &str, numeric: bool) -> String {
    let re = key.replace('.', "\\.");
    if numeric {
        format!(
            "CASE WHEN regexp_like(raw_attributes_json, '\"{re}\":-?[0-9]+') THEN \
             CAST(regexp_replace(raw_attributes_json, '(?s)^.*\"{re}\":(-?[0-9]+).*$', '$1') AS BIGINT) \
             ELSE 0 END"
        )
    } else {
        format!(
            "CASE WHEN regexp_like(raw_attributes_json, '\"{re}\":\"') THEN \
             regexp_replace(raw_attributes_json, '(?s)^.*\"{re}\":\"([^\"]*)\".*$', '$1') \
             ELSE '-' END"
        )
    }
}

/// Where data went since `since_unix_nano`, by destination and purpose, from the ledger.
pub async fn report(store: &Store, since_unix_nano: u64) -> anyhow::Result<Vec<ReportRow>> {
    let sql = format!(
        "SELECT host, ip, jurisdiction, purpose, COUNT(*) AS calls, \
                SUM(CASE WHEN outcome = 'refused' THEN 1 ELSE 0 END) AS refused, \
                SUM(CASE WHEN outcome = 'error' THEN 1 ELSE 0 END) AS errors, \
                SUM(bytes_out) AS bytes_out, SUM(bytes_in) AS bytes_in, \
                MIN(start_unix_nano) AS first, MAX(start_unix_nano) AS last \
         FROM (SELECT {} AS host, {} AS ip, {} AS jurisdiction, {} AS purpose, {} AS outcome, \
                      {} AS bytes_out, {} AS bytes_in, start_unix_nano \
               FROM spans WHERE service_name = '{EGRESS_SERVICE}' \
                 AND start_unix_nano >= {since_unix_nano}) \
         GROUP BY host, ip, jurisdiction, purpose \
         ORDER BY bytes_out DESC, calls DESC",
        attr_sql("server.address", false),
        attr_sql("network.peer.address", false),
        attr_sql("evald.egress.jurisdiction", false),
        attr_sql("evald.egress.purpose", false),
        attr_sql("evald.egress.outcome", false),
        attr_sql("evald.egress.bytes_out", true),
        attr_sql("evald.egress.bytes_in", true),
    );
    let rows = crate::sql::query(store, &sql, 100_000).await?.rows;
    let u = |v: &serde_json::Value| {
        v.as_u64()
            .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
            .unwrap_or(0)
    };
    let s = |v: &serde_json::Value| v.as_str().unwrap_or("-").to_string();
    Ok(rows
        .iter()
        .map(|r| ReportRow {
            host: s(&r["host"]),
            ip: s(&r["ip"]),
            jurisdiction: s(&r["jurisdiction"]),
            purpose: s(&r["purpose"]),
            calls: u(&r["calls"]),
            refused: u(&r["refused"]),
            errors: u(&r["errors"]),
            bytes_out: u(&r["bytes_out"]),
            bytes_in: u(&r["bytes_in"]),
            first_unix_nano: u(&r["first"]),
            last_unix_nano: u(&r["last"]),
        })
        .collect())
}

fn utc(ns: u64) -> String {
    chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, 0)
        .map_or_else(|| "-".into(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

/// `evald egress report`.
pub async fn report_command(data_dir: &Path, since: std::time::Duration) -> anyhow::Result<()> {
    let store = Store::open(
        data_dir,
        StoreConfig {
            compact_interval: None,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("opening store at {}: {e}", data_dir.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let since_ns = now.saturating_sub(since.as_nanos().min(u64::MAX as u128) as u64);
    let rows = report(&store, since_ns).await?;
    println!(
        "\nevald egress report — the last {:.1} d, from evald's own ledger",
        since.as_secs_f64() / 86_400.0
    );
    if rows.is_empty() {
        println!(
            "  no outbound call recorded{}",
            if cfg!(feature = "judge") {
                ""
            } else {
                " (and this build has no HTTP client: it can make none)"
            }
        );
        return Ok(());
    }
    println!(
        "{:<30} {:<16} {:<12} {:<13} {:>6} {:>7} {:>6} {:>10} {:>10}  first / last (UTC)",
        "host",
        "address",
        "jurisdiction",
        "purpose",
        "calls",
        "refused",
        "errors",
        "sent",
        "received"
    );
    for r in &rows {
        println!(
            "{:<30} {:<16} {:<12} {:<13} {:>6} {:>7} {:>6} {:>10} {:>10}  {} / {}",
            r.host,
            r.ip,
            r.jurisdiction,
            r.purpose,
            r.calls,
            r.refused,
            r.errors,
            crate::disk::human_bytes(r.bytes_out),
            crate::disk::human_bytes(r.bytes_in),
            utc(r.first_unix_nano),
            utc(r.last_unix_nano)
        );
    }
    println!(
        "\nWhat evald recorded of the calls it made, not a compliance claim. `refused` calls were \
         stopped by --egress-allow or --offline before they connected."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn allow_entries_parse_and_match() {
        let net = Allow::parse("10.0.0.0/8").unwrap();
        assert!(net.admits_ip(ip("10.200.3.4")));
        assert!(!net.admits_ip(ip("11.0.0.1")));
        let one = Allow::parse("127.0.0.1").unwrap();
        assert_eq!(one, Allow::Net(ip("127.0.0.1"), 32));
        assert!(one.admits_ip(ip("127.0.0.1")) && !one.admits_ip(ip("127.0.0.2")));
        let v6 = Allow::parse("fd00::/8").unwrap();
        assert!(v6.admits_ip(ip("fd12::1")) && !v6.admits_ip(ip("fe80::1")));
        assert!(!v6.admits_ip(ip("10.0.0.1")), "families never cross");
        assert!(Allow::parse("0.0.0.0/0").unwrap().admits_ip(ip("8.8.8.8")));
        assert_eq!(
            Allow::parse("LLM.Internal").unwrap(),
            Allow::Host("llm.internal".into())
        );
        for bad in ["", "10.0.0.0/33", "fd00::/129", "nope/8", "a b", "http://x"] {
            assert!(Allow::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_policy_admits_by_name_or_by_resolved_address() {
        let p = Policy::new(false, &["llm.internal".into(), "10.0.0.0/8".into()]).unwrap();
        assert_eq!(
            p.admit("LLM.internal", &[ip("192.168.1.5")]),
            Ok(Some(ip("192.168.1.5")))
        );
        // A name not listed is admitted by an address in a listed network, pinned to that one.
        assert_eq!(
            p.admit("gpu-box", &[ip("192.168.1.9"), ip("10.1.2.3")]),
            Ok(Some(ip("10.1.2.3")))
        );
        let err = p
            .admit("api.example.com", &[ip("203.0.113.7")])
            .unwrap_err();
        assert!(
            err.contains("203.0.113.7") && err.contains("--egress-allow"),
            "{err}"
        );
        assert!(
            p.admit("llm.internal", &[]).is_err(),
            "a listed name still has to resolve"
        );

        let off = Policy::new(true, &[]).unwrap();
        assert!(off
            .admit("localhost", &[ip("127.0.0.1")])
            .unwrap_err()
            .contains("--offline"));
        assert_eq!(
            Policy::default().admit("x", &[ip("1.2.3.4")]),
            Ok(Some(ip("1.2.3.4")))
        );
        assert!(
            Policy::new(true, &["10.0.0.0/8".into()]).is_err(),
            "one or the other"
        );
        assert_eq!(off.describe(), "offline: no outbound call");
        assert_eq!(p.describe(), "only llm.internal, 10.0.0.0/8");
    }

    fn rec(host: &str, outcome: &'static str, out: u64) -> Record {
        Record {
            start_unix_nano: 1_000,
            end_unix_nano: 2_000,
            endpoint: "box".into(),
            jurisdiction: Some("MY".into()),
            host: host.into(),
            port: 8000,
            ip: (outcome == "ok").then(|| ip("10.0.0.5")),
            method: "POST",
            path: "/v1/chat/completions".into(),
            bytes_out: out,
            bytes_in: 50,
            status: (outcome == "ok").then_some(200),
            model: Some("qwen".into()),
            purpose: Purpose::Judge,
            outcome,
        }
    }

    #[test]
    fn ledger_records_become_egress_spans_that_are_not_model_calls() {
        let l = Ledger::default();
        l.record(rec("gpu-box", "ok", 900));
        l.record(rec("api.example.com", "refused", 0));
        let records = l.drain();
        assert!(l.drain().is_empty());
        let spans = spans(&records, "run-1");
        assert_eq!(spans.len(), 2);
        let s = &spans[0];
        assert_eq!(s.service_name.as_deref(), Some(EGRESS_SERVICE));
        assert!(!s.is_llm_span(), "an egress record is not a model call");
        assert!(s.tokens.is_empty() && s.cost_usd.is_none());
        assert_eq!(s.raw_attributes["network.peer.address"], "10.0.0.5");
        assert_eq!(s.raw_attributes["evald.egress.jurisdiction"], "MY");
        assert_eq!(s.raw_attributes["evald.egress.bytes_out"], 900);
        assert_eq!(spans[1].status_code, 2, "a refusal is an error span");
        assert_ne!(spans[0].span_id, spans[1].span_id);
        assert!(super::spans(&[], "r").is_empty());
    }

    #[tokio::test]
    async fn the_report_sums_by_destination() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            StoreConfig {
                compact_interval: None,
                ..StoreConfig::default()
            },
        )
        .unwrap();
        let mut records = vec![rec("gpu-box", "ok", 900), rec("gpu-box", "ok", 100)];
        records[1].start_unix_nano = 5_000;
        records.push(rec("api.example.com", "refused", 0));
        store
            .ingest_batch(spans(&records, "run-1"), &[])
            .await
            .unwrap();
        let rows = report(&store, 0).await.unwrap();
        assert_eq!(rows.len(), 2, "{rows:#?}");
        let gpu = rows.iter().find(|r| r.host == "gpu-box").unwrap();
        assert_eq!(
            (gpu.calls, gpu.refused, gpu.bytes_out, gpu.bytes_in),
            (2, 0, 1000, 100)
        );
        assert_eq!(
            (gpu.ip.as_str(), gpu.jurisdiction.as_str()),
            ("10.0.0.5", "MY")
        );
        assert_eq!((gpu.first_unix_nano, gpu.last_unix_nano), (1_000, 5_000));
        let refused = rows.iter().find(|r| r.host == "api.example.com").unwrap();
        assert_eq!(
            (refused.calls, refused.refused, refused.ip.as_str()),
            (1, 1, "-")
        );
        assert!(
            report(&store, 10_000).await.unwrap().is_empty(),
            "the window applies"
        );
    }
}
