//! The on-disk format freeze (C12): a data-dir written by format 1 must keep opening,
//! reading and answering the same, release after release.
//!
//! `tests/fixtures/format-v1/` is a **committed** data-dir — WAL segment, flush blocks, a
//! merged block, the redb index and score store, and the `FORMAT` marker. Nothing in the
//! test regenerates it: it is a recording of what evald wrote at the freeze, and the tests
//! below read it with today's code. A change that makes them fail is a format break, and
//! the answer is a format version and a migration, not a new fixture.
//!
//! Regenerating is deliberately awkward and explicit — `EVALD_REGEN_FIXTURE=1 cargo test
//! --test format_freeze regenerate -- --ignored --nocapture` — and is only correct when the
//! format version itself has been bumped.
//!
//! The compatibility policy these pin (`docs/FORMAT.md`):
//!
//! - A release reads every data-dir at its own format **and the one before it**.
//! - A data-dir with no marker predates the freeze; it is stamped in place, never rewritten.
//! - A data-dir from a NEWER format is refused with an explanation, never opened
//!   optimistically — a store is never downgraded.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use evald::{Store, StoreConfig};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-v1")
}

/// Copy the committed fixture into a temp dir. Every test works on its own copy: opening a
/// store mutates the directory (it stamps, sweeps and may compact), and the fixture is a
/// recording that must not change.
fn copy_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("data");
    copy_tree(&fixture_dir(), &dst);
    (tmp, dst)
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// Every file under `dir`, as `/`-joined relative paths, sorted.
fn inventory(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

/// A content digest, so a mismatch reports a short hash instead of a megabyte of bytes.
fn digest(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    // FNV-1a: no dependency, and this only has to detect "these bytes changed".
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}/{}", bytes.len())
}

/// Open without a background compactor or merger: the fixture must be read exactly as it
/// was recorded, not quietly rewritten by the thing under test.
fn read_only_config() -> StoreConfig {
    StoreConfig {
        compact_interval: None,
        merge: evald::store::MergePolicy {
            hour_threshold: 0,
            day_merge: false,
            ..evald::store::MergePolicy::default()
        },
        ..StoreConfig::default()
    }
}

// --- what the fixture contains (asserted, not described) --------------------------------

/// Spans committed to cold blocks, as `(trace, span)`.
const COLD_SPANS: &[(&str, &str)] = &[
    ("t-alpha", "a1"),
    ("t-alpha", "a2"),
    ("t-beta", "b1"),
    ("t-gamma", "g1"),
];
/// A span left in the WAL above the watermark — it proves the WAL replays across releases.
const HOT_SPAN: (&str, &str) = ("t-delta", "d1");

/// The frozen layout, file for file. A new file appearing here is a format change.
const LAYOUT: &[&str] = &[
    "FORMAT",
    "blocks/2023/11/14/22/merged-00000000000000000001-00000000000000000002.parquet",
    "blocks/2023/11/14/23/00000000000000000003-0.parquet",
    "index.redb",
    "scores.redb",
    // The sealed segment holding the un-compacted span, and the empty active segment every
    // open creates after it — both are part of what a live data-dir looks like.
    "wal/00000000000000000004.wal",
    "wal/00000000000000000005.wal",
];

#[tokio::test]
async fn the_frozen_layout_has_not_drifted() {
    assert_eq!(
        inventory(&fixture_dir()),
        LAYOUT,
        "the committed format-1 fixture changed — if this is intentional the on-disk \
         FORMAT version must be bumped and a migration written (docs/FORMAT.md)"
    );
}

#[tokio::test]
async fn a_frozen_data_dir_still_opens_and_reads() {
    let (_tmp, dir) = copy_fixture();
    let store = Store::open(&dir, read_only_config()).unwrap();

    // Cold blocks and the replayed WAL segment together, deduped, as the API serves them.
    let mut seen: Vec<(String, String)> = store
        .query(None, 100)
        .unwrap()
        .into_iter()
        .map(|s| (s.trace_id, s.span_id))
        .collect();
    seen.sort();
    let mut want: Vec<(String, String)> = COLD_SPANS
        .iter()
        .chain(std::iter::once(&HOT_SPAN))
        .map(|(t, s)| (t.to_string(), s.to_string()))
        .collect();
    want.sort();
    assert_eq!(seen, want, "a format-1 data-dir reads back exactly");

    // The trace index still resolves (the redb multimap, not a scan).
    assert_eq!(store.trace("t-alpha").unwrap().len(), 2);
    assert_eq!(store.trace("t-delta").unwrap().len(), 1, "from the WAL");

    // Payloads survive, not just identities.
    let alpha = store.trace("t-alpha").unwrap();
    assert_eq!(alpha[0].model.as_deref(), Some("gpt-4o"));
    assert_eq!(alpha[0].tokens.prompt, Some(11));
    assert_eq!(alpha[0].input_value.as_deref(), Some("hello"));

    // Scores survive with their targets.
    let scores = store
        .scores_for_target(&evald::ScoreTarget::Span("a1".into()))
        .unwrap();
    assert_eq!(scores.len(), 1);
    assert_eq!(scores[0].name, "helpfulness");

    // The blocks still carry their provenance, including which blocks the merge replaced.
    let (prov, rows) = evald::store::cold::read_provenance(
        &dir.join("blocks/2023/11/14/22/merged-00000000000000000001-00000000000000000002.parquet"),
    )
    .unwrap();
    assert_eq!(prov.kind, evald::store::cold::BlockKind::Merged);
    assert_eq!((prov.seqno_lo, prov.seqno_hi, rows), (1, 2, 3));
    assert_eq!(
        prov.merged_from,
        vec![
            "blocks/2023/11/14/22/00000000000000000001-0.parquet",
            "blocks/2023/11/14/22/00000000000000000002-0.parquet",
        ]
    );
    let (flush, _) = evald::store::cold::read_provenance(
        &dir.join("blocks/2023/11/14/23/00000000000000000003-0.parquet"),
    )
    .unwrap();
    assert_eq!(flush.kind, evald::store::cold::BlockKind::Flush);
    assert_eq!(flush.seqno_lo, 3);
}

#[tokio::test]
async fn a_frozen_data_dir_still_answers_sql() {
    let (_tmp, dir) = copy_fixture();
    let store = Store::open(&dir, read_only_config()).unwrap();
    let result = evald::sql::query(&store, "SELECT COUNT(*) AS n FROM spans", 10)
        .await
        .unwrap();
    assert_eq!(result.rows[0]["n"], 5);
    let by_model = evald::sql::query(
        &store,
        "SELECT model, COUNT(*) AS n FROM spans WHERE model IS NOT NULL GROUP BY model",
        10,
    )
    .await
    .unwrap();
    assert_eq!(by_model.rows.len(), 1);
    assert_eq!(by_model.rows[0]["model"], "gpt-4o");
}

#[tokio::test]
async fn a_data_dir_written_before_the_marker_is_stamped_in_place() {
    let (_tmp, dir) = copy_fixture();
    fs::remove_file(dir.join("FORMAT")).unwrap(); // as a pre-freeze release left it
    let before = inventory(&dir);

    let store = Store::open(&dir, read_only_config()).expect("a pre-marker dir opens");
    assert_eq!(store.query(None, 100).unwrap().len(), 5, "data untouched");
    drop(store);

    let marker: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("FORMAT")).unwrap()).unwrap();
    assert_eq!(marker["format"], 1);
    assert_eq!(
        marker["stamped_from"], "legacy",
        "the marker records that it was added to an existing directory"
    );
    // Stamping is a marker write, not a migration. Nothing that was there is gone, nothing
    // appeared but the marker, and every Parquet block is byte-for-byte what it was — the
    // spans were not re-encoded to be readable.
    //
    // The redb files and the WAL are deliberately excluded from the byte check: opening a
    // store writes to them by design (redb checkpoints, and a fresh active segment is
    // opened). Blocks are the part of the format a migration would have had to touch.
    let after = inventory(&dir);
    for f in &before {
        assert!(after.contains(f), "{f} disappeared during stamping");
        if f.starts_with("blocks/") {
            assert_eq!(
                digest(&fixture_dir().join(f)),
                digest(&dir.join(f)),
                "{f} was rewritten during stamping"
            );
        }
    }
    assert!(
        after
            .iter()
            .all(|f| before.contains(f) || f == "FORMAT" || f.starts_with("wal/")),
        "stamping added something other than the marker: {after:?}"
    );
}

#[tokio::test]
async fn a_newer_on_disk_format_is_refused_with_an_explanation() {
    let (_tmp, dir) = copy_fixture();
    let mut marker: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("FORMAT")).unwrap()).unwrap();
    marker["format"] = serde_json::json!(2);
    marker["evald_version"] = serde_json::json!("9.9.9");
    fs::write(dir.join("FORMAT"), marker.to_string()).unwrap();

    let msg = match Store::open(&dir, read_only_config()) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a newer on-disk format must be refused, not opened"),
    };
    for want in [
        "format 2",
        "9.9.9",
        "reads formats up to 1",
        "Upgrade evald",
    ] {
        assert!(msg.contains(want), "{want:?} missing from: {msg}");
    }
}

/// `evald migrate` is the operator-facing half of the same policy.
#[tokio::test]
async fn evald_migrate_reports_and_stamps() {
    let (_tmp, dir) = copy_fixture();
    fs::remove_file(dir.join("FORMAT")).unwrap();
    let migrate = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_evald"))
            .arg("migrate")
            .arg("--data-dir")
            .arg(&dir)
            .args(args)
            .output()
            .expect("spawn evald migrate")
    };

    let out = migrate(&["--dry-run"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("WOULD be stamped"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!dir.join("FORMAT").exists(), "a dry run writes nothing");

    let out = migrate(&[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("stamped"));
    assert!(dir.join("FORMAT").exists());

    // Idempotent: a second run has nothing to do and says so.
    let out = migrate(&[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to do"));

    // And it refuses a newer format rather than "migrating" one it cannot read.
    let mut marker: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("FORMAT")).unwrap()).unwrap();
    marker["format"] = serde_json::json!(2);
    fs::write(dir.join("FORMAT"), marker.to_string()).unwrap();
    let out = migrate(&[]);
    assert!(!out.status.success(), "a newer format exits non-zero");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Upgrade evald"));
}

// --- regeneration (only after a deliberate format bump) ----------------------------------

/// Rewrite `tests/fixtures/format-v1/`. NOT part of the suite: it is the recording step,
/// and re-recording is how a format break hides itself. Run it only when the on-disk format
/// version has been bumped on purpose, and commit the result as the new frozen fixture.
#[tokio::test]
#[ignore = "regenerates the committed fixture; only correct after a deliberate format bump"]
async fn regenerate() {
    assert_eq!(
        std::env::var("EVALD_REGEN_FIXTURE").as_deref(),
        Ok("1"),
        "set EVALD_REGEN_FIXTURE=1 to confirm you mean to re-record the frozen format"
    );
    let dir = fixture_dir();
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();

    let store = Store::open(&dir, read_only_config()).unwrap();
    // Two flush blocks in hour 22, one in hour 23 — then merge 22 so the fixture carries a
    // merged block and a flush block side by side.
    let h22: u64 = 1_700_000_000_000_000_000; // 2023-11-14T22:13:20Z
    let h23: u64 = h22 + 3_600_000_000_000;
    let mut rich = span("t-alpha", "a1", h22);
    rich.model = Some("gpt-4o".into());
    rich.provider = Some("openai".into());
    rich.tokens.prompt = Some(11);
    rich.tokens.completion = Some(7);
    rich.tokens.total = Some(18);
    rich.cost_usd = Some(0.000_25);
    rich.input_value = Some("hello".into());
    rich.output_value = Some("hi there".into());
    rich.session_id = Some("sess-1".into());
    rich.service_name = Some("checkout".into());
    rich.raw_attributes
        .insert("custom.flag".into(), serde_json::json!(true));

    for batch in [
        vec![rich, span("t-alpha", "a2", h22 + 1)],
        vec![span("t-beta", "b1", h22 + 2)],
        vec![span("t-gamma", "g1", h23)],
    ] {
        store.append(batch).await.unwrap();
        store.seal_now().await.unwrap();
        store.compact_now().await.unwrap();
    }
    store
        .merge_now(evald::store::MergeScope::Full, false)
        .await
        .unwrap();
    store
        .sweep_unindexed(std::time::Duration::ZERO)
        .await
        .unwrap();

    store
        .put_scores(&[evald::Score {
            id: "sc-1".into(),
            target: evald::ScoreTarget::Span("a1".into()),
            name: "helpfulness".into(),
            num_value: Some(0.8),
            str_value: None,
            data_type: evald::model::DataType::Numeric,
            source: evald::model::ScoreSource::Human,
            comment: Some("clear answer".into()),
            config_id: None,
            agg_stats: None,
            ts_unix_nano: h22,
        }])
        .unwrap();

    // One span left un-compacted, so the fixture carries a WAL segment to replay.
    store
        .append(vec![span(HOT_SPAN.0, HOT_SPAN.1, h23 + 1)])
        .await
        .unwrap();
    store.seal_now().await.unwrap();
    drop(store);

    // `blobs/` is created empty by every open and carries nothing here; leaving it out keeps
    // the fixture to files that actually encode format.
    let _ = fs::remove_dir(dir.join("blobs"));
    println!("regenerated {}:", dir.display());
    for f in inventory(&dir) {
        println!("  {f}");
    }
}

/// Minimal span, matching the shape the OTLP normalizer produces.
fn span(trace_id: &str, span_id: &str, start_unix_nano: u64) -> evald::NormalizedSpan {
    evald::NormalizedSpan {
        dialect: evald::model::Dialect::Unknown,
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        name: "llm".to_string(),
        otel_kind: 1,
        oi_kind: None,
        start_unix_nano,
        end_unix_nano: start_unix_nano + 1_000,
        status_code: 0,
        status_message: None,
        model: None,
        provider: None,
        tokens: evald::model::Tokens::default(),
        cost_usd: None,
        input_value: None,
        output_value: None,
        session_id: None,
        user_id: None,
        service_name: None,
        scope_name: None,
        scope_version: None,
        raw_attributes: Default::default(),
    }
}
