//! `evald doctor`: check a setup before (or instead of) finding out from a failed start.
//!
//! Each check says what it found and, when something is off, the fix. `fail` means `serve`
//! would not work or would be unsafe as configured; `warn` means it would work but probably
//! not as intended. The exit code is `1` when any check fails (a gate, like the others),
//! `0` otherwise.
//!
//! Nothing here connects anywhere. A port is checked by binding it, and the data dir by
//! writing a probe file into it; neither touches a running server or its store.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

/// One check's verdict.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
        fix: None,
    }
}

impl Check {
    fn fix(mut self, fix: impl Into<String>) -> Check {
        self.fix = Some(fix.into());
        self
    }
}

/// What `serve` would be started with.
pub struct Setup<'a> {
    pub data_dir: &'a Path,
    pub otlp_http: &'a str,
    pub otlp_grpc: &'a str,
    pub auth_configured: bool,
    /// The `--price-table` file `serve` would lay over the built-in table.
    pub price_table: Option<&'a Path>,
}

/// The serve default for `--disk-min-free`: below it, ingest is refused.
const DISK_FLOOR: u64 = 256 * 1024 * 1024;
/// Below this, warn: the floor is close.
const DISK_LOW: u64 = 1024 * 1024 * 1024;

/// Run every check.
pub fn run_checks(s: &Setup) -> Vec<Check> {
    let mut out = data_dir_checks(s.data_dir);
    let http = port_check("http port", s.otlp_http);
    let grpc = if s.otlp_grpc.trim().is_empty() {
        (
            check("grpc port", Level::Ok, "disabled (--otlp-grpc is empty)"),
            None,
        )
    } else {
        port_check("grpc port", s.otlp_grpc)
    };
    let addrs: Vec<SocketAddr> = [http.1, grpc.1].into_iter().flatten().collect();
    out.push(http.0);
    out.push(grpc.0);
    out.push(exposure_check(&addrs, s.auth_configured));
    out.push(price_check(s.price_table));
    out.push(outbound_check());
    out
}

fn data_dir_checks(dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let shown = dir.display().to_string();
    if !dir.exists() {
        out.push(check(
            "data dir",
            Level::Ok,
            format!("{shown} does not exist yet; `evald serve` creates it"),
        ));
    } else if !dir.is_dir() {
        out.push(
            check(
                "data dir",
                Level::Fail,
                format!("{shown} is not a directory"),
            )
            .fix("point --data-dir (EVALD_DATA_DIR) at a directory"),
        );
        return out;
    } else {
        let probe = dir.join(format!(".evald-doctor-{}", std::process::id()));
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                out.push(check("data dir", Level::Ok, format!("{shown} is writable")));
            }
            Err(e) => out.push(
                check(
                    "data dir",
                    Level::Fail,
                    format!("{shown} is not writable: {e}"),
                )
                .fix(
                    "give the user evald runs as write access (in a container: \
                     --user \"$(id -u):$(id -g)\" on a directory you own, or a named volume)",
                ),
            ),
        }
    }
    let abs = dir
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(dir));
    if crate::ingest::is_ephemeral_path(&abs) {
        out.push(
            check(
                "persistence",
                Level::Warn,
                format!(
                    "{} looks ephemeral: spans will not survive a reboot",
                    abs.display()
                ),
            )
            .fix("put --data-dir on persistent storage (e.g. /var/lib/evald, or a volume)"),
        );
    }
    // Free space where the store is, or would be.
    let probe_dir = std::iter::successors(Some(abs.as_path()), |p| p.parent()).find(|p| p.exists());
    match probe_dir.and_then(crate::disk::free_bytes) {
        Some(free) if free < DISK_FLOOR => out.push(
            check(
                "disk",
                Level::Fail,
                format!(
                    "{} free: under the {} floor, so ingest would be refused",
                    crate::disk::human_bytes(free),
                    crate::disk::human_bytes(DISK_FLOOR)
                ),
            )
            .fix("free space, enable --retention, or move --data-dir"),
        ),
        Some(free) if free < DISK_LOW => out.push(
            check(
                "disk",
                Level::Warn,
                format!(
                    "{} free: close to the ingest floor",
                    crate::disk::human_bytes(free)
                ),
            )
            .fix("free space or enable --retention before it fills"),
        ),
        Some(free) => out.push(check(
            "disk",
            Level::Ok,
            format!("{} free", crate::disk::human_bytes(free)),
        )),
        None => out.push(check("disk", Level::Warn, "free space could not be read")),
    }
    out
}

fn port_check(name: &'static str, addr: &str) -> (Check, Option<SocketAddr>) {
    let parsed: SocketAddr = match addr.trim().parse() {
        Ok(a) => a,
        Err(e) => {
            return (
                check(
                    name,
                    Level::Fail,
                    format!("{addr:?} is not a host:port address: {e}"),
                )
                .fix("use e.g. 127.0.0.1:4318"),
                None,
            )
        }
    };
    let c = match TcpListener::bind(parsed) {
        Ok(listener) => {
            drop(listener);
            check(name, Level::Ok, format!("{parsed} is free"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => check(
            name,
            Level::Warn,
            format!("{parsed} is in use (an `evald serve` already running there?)"),
        )
        .fix("stop the other process, or pick another port"),
        Err(e) => check(name, Level::Fail, format!("cannot bind {parsed}: {e}"))
            .fix("pick a port this user may bind (above 1024 without privileges)"),
    };
    (c, Some(parsed))
}

fn exposure_check(addrs: &[SocketAddr], auth: bool) -> Check {
    let exposed: Vec<String> = addrs
        .iter()
        .filter(|a| !a.ip().is_loopback())
        .map(|a| a.to_string())
        .collect();
    match (exposed.is_empty(), auth) {
        (true, _) => check(
            "exposure",
            Level::Ok,
            "loopback only: reachable from this machine",
        ),
        (false, true) => check(
            "exposure",
            Level::Ok,
            format!(
                "{} beyond this machine, behind the bearer-token gate",
                exposed.join(", ")
            ),
        )
        .fix("evald does no TLS: terminate it at a reverse proxy on an untrusted network"),
        (false, false) => check(
            "exposure",
            Level::Fail,
            format!(
                "{} beyond this machine with NO authentication: anyone who can reach it can \
                 read and write every span",
                exposed.join(", ")
            ),
        )
        .fix("set --auth-token / EVALD_AUTH_TOKEN (16+ printable characters), or bind 127.0.0.1"),
    }
}

/// The table `serve` would price with: the built-in one, or `file` laid over it. A file that
/// does not load is a failure, since `serve` refuses to start with it.
fn price_check(file: Option<&Path>) -> Check {
    let table = match file {
        None => crate::price::current(),
        Some(f) => match crate::price::PriceTable::load_over_baseline(f) {
            Ok(t) => std::sync::Arc::new(t),
            Err(e) => {
                return check("price table", Level::Fail, format!("{e:#}"))
                    .fix("fix the --price-table file: serve refuses to start with it")
            }
        },
    };
    let as_of = table
        .as_of
        .map(|d| d.to_string())
        .unwrap_or_else(|| "unknown date".into());
    match table.stale_days(crate::price::today()) {
        Some(days) => check(
            "price table",
            Level::Warn,
            format!(
                "{} is {days} days old: new models are unpriced",
                table.version
            ),
        )
        .fix("pass --price-table <file> with current prices (no network needed at runtime)"),
        None => check(
            "price table",
            Level::Ok,
            format!("{} (as of {as_of})", table.version),
        ),
    }
}

fn outbound_check() -> Check {
    if cfg!(feature = "judge") {
        check(
            "outbound calls",
            Level::Ok,
            "the judge feature is built in: evald calls a provider only when you run a judge",
        )
    } else {
        check(
            "outbound calls",
            Level::Ok,
            "none: this build has no network client (CI-checked)",
        )
    }
}

/// Print the report; true when nothing failed.
pub fn print(checks: &[Check]) -> bool {
    println!("evald doctor ({})\n", env!("CARGO_PKG_VERSION"));
    for c in checks {
        println!("  {:<5} {:<15} {}", c.level.label(), c.name, c.detail);
        if let (Some(fix), true) = (&c.fix, c.level != Level::Ok) {
            println!("  {:<5} {:<15} → {fix}", "", "");
        }
    }
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    println!(
        "\n{}",
        match (fails, warns) {
            (0, 0) => "all good. Next: evald serve, then evald connect <tool>.".to_string(),
            (0, w) => format!("{w} warning(s); evald serve will run."),
            (f, _) => format!("{f} problem(s) to fix before evald serve."),
        }
    );
    fails == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn a_fresh_loopback_setup_is_all_ok() {
        let dir = tempfile::tempdir().unwrap();
        let checks = run_checks(&Setup {
            data_dir: &dir.path().join("store"),
            otlp_http: "127.0.0.1:0",
            otlp_grpc: "127.0.0.1:0",
            auth_configured: false,
            price_table: None,
        });
        for name in [
            "data dir",
            "http port",
            "grpc port",
            "exposure",
            "outbound calls",
        ] {
            assert_eq!(
                by_name(&checks, name).level,
                Level::Ok,
                "{name}: {checks:?}"
            );
        }
    }

    #[test]
    fn exposing_without_auth_fails_and_with_auth_passes() {
        let open: SocketAddr = "0.0.0.0:4318".parse().unwrap();
        assert_eq!(exposure_check(&[open], false).level, Level::Fail);
        assert_eq!(exposure_check(&[open], true).level, Level::Ok);
    }

    #[test]
    fn a_port_in_use_is_named() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap().to_string();
        let (c, _) = port_check("http port", &addr);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("in use"), "{}", c.detail);
        let (bad, _) = port_check("http port", "nonsense");
        assert_eq!(bad.level, Level::Fail);
    }

    #[test]
    fn a_file_where_the_data_dir_should_be_fails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let checks = data_dir_checks(&file);
        assert_eq!(by_name(&checks, "data dir").level, Level::Fail);
    }

    /// `serve --price-table` prices with that file over the built-in table: doctor grades the
    /// same table, and a file serve would refuse is a failure.
    #[test]
    fn the_price_table_checked_is_the_one_serve_would_use() {
        let dir = tempfile::tempdir().unwrap();
        let run = |file: &Path| {
            run_checks(&Setup {
                data_dir: dir.path(),
                otlp_http: "127.0.0.1:0",
                otlp_grpc: "",
                auth_configured: false,
                price_table: Some(file),
            })
        };
        let fresh = dir.path().join("prices.json");
        let today = crate::price::today();
        std::fs::write(
            &fresh,
            format!(
                r#"{{"_evald": {{"commit": "mine", "date": "{today}"}},
                   "my-model": {{"input_cost_per_token": 1e-6, "output_cost_per_token": 2e-6}}}}"#
            ),
        )
        .unwrap();
        let checks = run(&fresh);
        let c = by_name(&checks, "price table");
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert!(
            c.detail.contains("mine"),
            "the operator's table: {}",
            c.detail
        );

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert_eq!(by_name(&run(&broken), "price table").level, Level::Fail);
    }

    #[test]
    fn an_empty_grpc_address_means_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let checks = run_checks(&Setup {
            data_dir: dir.path(),
            otlp_http: "127.0.0.1:0",
            otlp_grpc: "",
            auth_configured: false,
            price_table: None,
        });
        assert!(by_name(&checks, "grpc port").detail.contains("disabled"));
    }
}
