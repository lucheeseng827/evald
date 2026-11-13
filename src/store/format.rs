//! The on-disk format marker — `FORMAT` at the data-dir root (`docs/FORMAT.md`).
//!
//! One small JSON file that says which format the directory is in, so a binary can refuse a
//! directory it does not understand *before* it touches anything, instead of misreading it.
//! The rules it enforces:
//!
//! - **A release reads its own format and the one before it.** Format 1 is the layout
//!   v0.2.x wrote (unmarked — "legacy"), plus this marker and the provenance metadata new
//!   blocks carry. A legacy directory is therefore stamped in place on first open, with
//!   nothing else rewritten; there is no data to migrate, only a file to add.
//! - **A directory is never downgraded.** A marker newer than this build refuses to open,
//!   with the sentence an operator needs: which format, which version wrote it, upgrade.
//! - **Stamping is atomic** (temp → fsync → rename → directory fsync) and happens once.
//!   Every later open reads the marker and leaves it alone.
//!
//! The marker is deliberately tiny and self-describing. `evald_version` and
//! `created_unix_ms` are informational — nothing decides on them; only `format` does.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The format this build writes, and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;

/// The marker file's name, at the data-dir root.
pub const FORMAT_FILE: &str = "FORMAT";

/// What `FORMAT` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatMarker {
    /// The on-disk format number. The only field anything decides on.
    pub format: u32,
    /// The evald version that wrote the marker. Informational.
    pub evald_version: String,
    /// When the marker was written. Informational.
    pub created_unix_ms: u64,
    /// `"legacy"` when the directory predated the marker and was stamped in place on open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamped_from: Option<String>,
}

/// What a data-dir looks like before it is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatState {
    /// Nothing evald wrote yet: an empty or absent directory.
    Fresh,
    /// A data-dir written before the marker existed (v0.2.x). Format 1 in all but name.
    Legacy,
    /// A marked directory.
    Marked(FormatMarker),
}

/// Read the marker without changing anything.
pub fn inspect(data_dir: &Path) -> io::Result<FormatState> {
    let path = data_dir.join(FORMAT_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let marker: FormatMarker = serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is not a valid format marker ({e}); refusing to guess what this \
                         directory is",
                        path.display()
                    ),
                )
            })?;
            Ok(FormatState::Marked(marker))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Anything the engine ever wrote means a legacy store; otherwise it is new.
            let has_state = ["wal", "blocks", "index.redb", "scores.redb"]
                .iter()
                .any(|p| data_dir.join(p).exists());
            Ok(if has_state {
                FormatState::Legacy
            } else {
                FormatState::Fresh
            })
        }
        Err(e) => Err(e),
    }
}

/// Whether this build may open a directory carrying `marker`.
pub fn check_readable(marker: &FormatMarker, data_dir: &Path) -> io::Result<()> {
    if marker.format == 0 || marker.format > FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "data-dir {} is on-disk format {}, written by evald {}; this evald ({}) reads \
                 formats up to {}. Upgrade evald — a data-dir is never downgraded (docs/FORMAT.md).",
                data_dir.display(),
                marker.format,
                marker.evald_version,
                env!("CARGO_PKG_VERSION"),
                FORMAT_VERSION
            ),
        ));
    }
    Ok(())
}

/// The check every open runs first: refuse a newer format, stamp a legacy or fresh
/// directory, and return the marker.
pub fn ensure(data_dir: &Path) -> io::Result<FormatMarker> {
    match inspect(data_dir)? {
        FormatState::Marked(marker) => {
            check_readable(&marker, data_dir)?;
            Ok(marker)
        }
        FormatState::Legacy => {
            let marker = stamp(data_dir, Some("legacy"))?;
            tracing::info!(
                path = %data_dir.join(FORMAT_FILE).display(),
                "stamped a pre-marker data-dir as on-disk format {FORMAT_VERSION} (nothing else changed)"
            );
            Ok(marker)
        }
        FormatState::Fresh => stamp(data_dir, None),
    }
}

/// Write the marker atomically. `stamped_from` records where the directory came from.
pub fn stamp(data_dir: &Path, stamped_from: Option<&str>) -> io::Result<FormatMarker> {
    fs::create_dir_all(data_dir)?;
    let marker = FormatMarker {
        format: FORMAT_VERSION,
        evald_version: env!("CARGO_PKG_VERSION").to_string(),
        created_unix_ms: now_unix_ms(),
        stamped_from: stamped_from.map(str::to_string),
    };
    let mut json = serde_json::to_string_pretty(&marker).map_err(io::Error::other)?;
    json.push('\n');
    let tmp = data_dir.join(format!("{FORMAT_FILE}.tmp"));
    fs::write(&tmp, json)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, data_dir.join(FORMAT_FILE))?;
    fsync_dir(data_dir)?;
    Ok(marker)
}

/// How a preflight line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    Ok,
    /// Worth acting on before the upgrade, not a reason it cannot happen.
    Warn,
    /// This evald cannot take over the directory.
    Fail,
}

/// One line of `evald migrate --check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: CheckLevel,
    pub name: &'static str,
    pub detail: String,
}

/// Whether this evald can take over `data_dir`, writing nothing: the on-disk format (read),
/// whether a running evald holds the store (a shared lock probe on the index, released at
/// once), and the free space beside it. The upgrade preflight: run the new binary's `migrate
/// --check` against the old one's data-dir before switching.
pub fn preflight(data_dir: &Path) -> Vec<Check> {
    let check = |level, name, detail: String| Check {
        level,
        name,
        detail,
    };
    let this = env!("CARGO_PKG_VERSION");
    let mut out = Vec::new();
    out.push(match inspect(data_dir) {
        Ok(FormatState::Fresh) => check(
            CheckLevel::Ok,
            "format",
            format!(
                "nothing here yet: evald {this} creates format {FORMAT_VERSION} on first start"
            ),
        ),
        Ok(FormatState::Legacy) => check(
            CheckLevel::Warn,
            "format",
            format!(
                "written before the format marker (v0.2.x): stamped as format {FORMAT_VERSION} \
                 on first start, or now with `evald migrate`; no data is rewritten"
            ),
        ),
        Ok(FormatState::Marked(m)) => match check_readable(&m, data_dir) {
            Ok(()) => check(
                CheckLevel::Ok,
                "format",
                format!(
                    "on-disk format {}, written by evald {}; evald {this} reads formats up to \
                     {FORMAT_VERSION}",
                    m.format, m.evald_version
                ),
            ),
            Err(e) => check(CheckLevel::Fail, "format", e.to_string()),
        },
        Err(e) => check(CheckLevel::Fail, "format", e.to_string()),
    });

    let index = data_dir.join("index.redb");
    out.push(match File::open(&index) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            check(CheckLevel::Ok, "in use", "no store to hold yet".into())
        }
        Err(e) => check(
            CheckLevel::Fail,
            "in use",
            format!("cannot read {}: {e}", index.display()),
        ),
        Ok(f) => match f.try_lock_shared() {
            Ok(()) => {
                let _ = f.unlock();
                check(
                    CheckLevel::Ok,
                    "in use",
                    "no running evald holds the store".into(),
                )
            }
            Err(fs::TryLockError::WouldBlock) => check(
                CheckLevel::Warn,
                "in use",
                "a running evald holds the store: stop it before starting this one (an evald \
                 refuses a store another holds)"
                    .into(),
            ),
            Err(fs::TryLockError::Error(e)) => check(
                CheckLevel::Warn,
                "in use",
                format!("could not probe the store's lock: {e}"),
            ),
        },
    });

    let size = dir_size(data_dir);
    // A data-dir that does not exist yet will be made on the nearest one that does.
    let existing = data_dir
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("."));
    out.push(match crate::disk::free_bytes(existing) {
        Some(free) if free < size => check(
            CheckLevel::Warn,
            "disk",
            format!(
                "{} free beside a {} store: compaction rewrites blocks, so keep at least the \
                 store's size free",
                crate::disk::human_bytes(free),
                crate::disk::human_bytes(size)
            ),
        ),
        Some(free) => check(
            CheckLevel::Ok,
            "disk",
            format!(
                "{} free, the store is {}",
                crate::disk::human_bytes(free),
                crate::disk::human_bytes(size)
            ),
        ),
        None => check(
            CheckLevel::Warn,
            "disk",
            format!(
                "free space unknown here; the store is {}",
                crate::disk::human_bytes(size)
            ),
        ),
    });
    out
}

/// The bytes of every file under `dir` (0 when it does not exist).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_upgrade_preflight_reads_and_writes_nothing() {
        use super::{preflight, CheckLevel::*};
        let level = |checks: &[super::Check], name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.level)
                .unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Empty: fine, and still empty afterwards.
        let c = preflight(d);
        assert_eq!(
            (level(&c, "format"), level(&c, "in use")),
            (Ok, Ok),
            "{c:#?}"
        );
        assert_eq!(std::fs::read_dir(d).unwrap().count(), 0, "nothing written");

        // A pre-marker store: a warning, and it is not stamped by the check.
        std::fs::create_dir(d.join("wal")).unwrap();
        std::fs::write(d.join("index.redb"), b"").unwrap();
        assert_eq!(level(&preflight(d), "format"), Warn);
        assert!(
            !d.join(super::FORMAT_FILE).exists(),
            "the check stamps nothing"
        );

        // Marked and readable.
        super::stamp(d, None).unwrap();
        let c = preflight(d);
        assert_eq!(level(&c, "format"), Ok);
        assert!(c[0].detail.contains("format 1"), "{c:#?}");
        assert!(c.iter().any(|x| x.name == "disk"));

        // Held by a running evald (here: a lock on the index, as redb takes).
        let held = std::fs::File::open(d.join("index.redb")).unwrap();
        held.lock().unwrap();
        let c = preflight(d);
        assert_eq!(level(&c, "in use"), Warn, "{c:#?}");
        assert!(c[1].detail.contains("stop it"));
        held.unlock().unwrap();
        assert_eq!(
            level(&preflight(d), "in use"),
            Ok,
            "the probe left no lock behind"
        );

        // And a real store, open in this process the way `serve` holds it.
        let live_dir = tempfile::tempdir().unwrap();
        let store = crate::Store::open(
            live_dir.path(),
            crate::StoreConfig {
                compact_interval: None,
                ..crate::StoreConfig::default()
            },
        )
        .unwrap();
        assert_eq!(
            level(&preflight(live_dir.path()), "in use"),
            Warn,
            "redb's own lock is seen"
        );
        drop(store);
        assert_eq!(level(&preflight(live_dir.path()), "in use"), Ok);

        // Written by a newer evald: this one cannot take over.
        std::fs::write(
            d.join(super::FORMAT_FILE),
            r#"{"format": 99, "evald_version": "9.0.0", "created_unix_ms": 1}"#,
        )
        .unwrap();
        let c = preflight(d);
        assert_eq!(level(&c, "format"), Fail);
        assert!(c[0].detail.contains("never downgraded"), "{c:#?}");
    }

    use super::*;

    #[test]
    fn a_fresh_directory_is_stamped_without_a_legacy_note() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(inspect(dir.path()).unwrap(), FormatState::Fresh);
        let marker = ensure(dir.path()).unwrap();
        assert_eq!(marker.format, FORMAT_VERSION);
        assert_eq!(marker.stamped_from, None);
        assert!(dir.path().join(FORMAT_FILE).exists());
        assert!(!dir.path().join("FORMAT.tmp").exists());
        // A second open reads it back unchanged.
        assert_eq!(
            inspect(dir.path()).unwrap(),
            FormatState::Marked(marker.clone())
        );
        assert_eq!(ensure(dir.path()).unwrap(), marker);
    }

    #[test]
    fn a_legacy_directory_is_recognised_and_stamped_in_place() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wal")).unwrap();
        fs::write(dir.path().join("wal/00000000000000000001.wal"), b"").unwrap();
        assert_eq!(inspect(dir.path()).unwrap(), FormatState::Legacy);
        let marker = ensure(dir.path()).unwrap();
        assert_eq!(marker.format, FORMAT_VERSION);
        assert_eq!(marker.stamped_from.as_deref(), Some("legacy"));
        // The legacy state left the WAL alone.
        assert!(dir.path().join("wal/00000000000000000001.wal").exists());
    }

    #[test]
    fn a_newer_format_refuses_to_open_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FORMAT_FILE),
            r#"{"format": 2, "evald_version": "9.9.9", "created_unix_ms": 0}"#,
        )
        .unwrap();
        let err = ensure(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains("format 2"), "{msg}");
        assert!(msg.contains("9.9.9"), "{msg}");
        assert!(msg.contains("Upgrade evald"), "{msg}");
        // Format 0 is not a format either.
        fs::write(
            dir.path().join(FORMAT_FILE),
            r#"{"format": 0, "evald_version": "x", "created_unix_ms": 0}"#,
        )
        .unwrap();
        assert!(ensure(dir.path()).is_err());
    }

    #[test]
    fn an_unreadable_marker_is_an_error_not_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FORMAT_FILE), "not json").unwrap();
        let err = ensure(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("refusing to guess"), "{err}");
    }

    #[test]
    fn the_marker_tolerates_unknown_fields_from_a_later_minor() {
        // A later release may add informational fields without bumping `format`; this
        // build must keep reading the marker.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FORMAT_FILE),
            r#"{"format": 1, "evald_version": "1.4.0", "created_unix_ms": 1, "future_note": "x"}"#,
        )
        .unwrap();
        let marker = ensure(dir.path()).unwrap();
        assert_eq!(marker.format, 1);
        assert_eq!(marker.evald_version, "1.4.0");
    }
}
