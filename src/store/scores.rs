//! Score store — redb (PoC step 5).
//!
//! The universal [`Score`] object ([`crate::model`] §2.2) is persisted in its own redb
//! database (`scores.redb`): lower-volume than spans, point-lookup by target, and ACID
//! durability without duplicating the WAL/Parquet machinery. Two tables:
//!
//! - `scores`: `id -> JSON(Score)` — the score itself, upserted by id.
//! - `by_target`: `target_key -> { id }` — every score for a span/trace/session/run, so
//!   `GET /v1/scores?span_id=…` is a multimap point lookup.
//!
//! redb commits fsync, so a stored score survives a crash.

use std::io;
use std::path::Path;

use redb::{Database, MultimapTableDefinition, ReadableDatabase, ReadableTable, TableDefinition};

use crate::model::Score;

const SCORES: TableDefinition<&str, &str> = TableDefinition::new("scores");
const BY_TARGET: MultimapTableDefinition<&str, &str> = MultimapTableDefinition::new("by_target");

pub struct ScoreStore {
    db: Database,
}

impl ScoreStore {
    /// Open (or create) the score database at `path`, materializing its tables.
    pub fn open(path: &Path) -> io::Result<ScoreStore> {
        let db = Database::create(path).map_err(io::Error::other)?;
        let txn = db.begin_write().map_err(io::Error::other)?;
        {
            txn.open_table(SCORES).map_err(io::Error::other)?;
            txn.open_multimap_table(BY_TARGET)
                .map_err(io::Error::other)?;
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(ScoreStore { db })
    }

    /// Upsert a batch of scores (one durable transaction). Re-putting the same id
    /// overwrites the score and, if its target changed, drops the stale
    /// `(old_target_key, id)` mapping so the score never resolves under a target it no
    /// longer belongs to.
    pub fn put_batch(&self, scores: &[Score]) -> io::Result<()> {
        if scores.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(io::Error::other)?;
        {
            let mut tbl = txn.open_table(SCORES).map_err(io::Error::other)?;
            let mut by_target = txn
                .open_multimap_table(BY_TARGET)
                .map_err(io::Error::other)?;
            for score in scores {
                let new_key = score.target.key();
                // If this id already exists under a different target, remove the old
                // mapping before re-inserting (multimap inserts never replace).
                let old_key = match tbl.get(score.id.as_str()).map_err(io::Error::other)? {
                    Some(existing) => Some(parse_score(existing.value())?.target.key()),
                    None => None,
                };
                if let Some(old_key) = old_key {
                    if old_key != new_key {
                        by_target
                            .remove(old_key.as_str(), score.id.as_str())
                            .map_err(io::Error::other)?;
                    }
                }
                let json = serde_json::to_string(score)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                tbl.insert(score.id.as_str(), json.as_str())
                    .map_err(io::Error::other)?;
                by_target
                    .insert(new_key.as_str(), score.id.as_str())
                    .map_err(io::Error::other)?;
            }
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(())
    }

    /// Fetch a single score by id.
    pub fn get(&self, id: &str) -> io::Result<Option<Score>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let tbl = txn.open_table(SCORES).map_err(io::Error::other)?;
        match tbl.get(id).map_err(io::Error::other)? {
            Some(v) => Ok(Some(parse_score(v.value())?)),
            None => Ok(None),
        }
    }

    /// All scores for a target key (e.g. `span:<hex>`), newest-first.
    pub fn by_target_key(&self, key: &str) -> io::Result<Vec<Score>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let scores = txn.open_table(SCORES).map_err(io::Error::other)?;
        let by_target = txn
            .open_multimap_table(BY_TARGET)
            .map_err(io::Error::other)?;
        let mut out = Vec::new();
        for id in by_target.get(key).map_err(io::Error::other)? {
            let id = id.map_err(io::Error::other)?;
            if let Some(v) = scores.get(id.value()).map_err(io::Error::other)? {
                out.push(parse_score(v.value())?);
            }
        }
        sort_newest_first(&mut out);
        Ok(out)
    }

    /// Recent scores across all targets, newest-first, capped at `limit`.
    pub fn list(&self, limit: usize) -> io::Result<Vec<Score>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let tbl = txn.open_table(SCORES).map_err(io::Error::other)?;
        let mut out = Vec::new();
        for entry in tbl.iter().map_err(io::Error::other)? {
            let (_id, v) = entry.map_err(io::Error::other)?;
            out.push(parse_score(v.value())?);
        }
        sort_newest_first(&mut out);
        out.truncate(limit);
        Ok(out)
    }

    /// Total scores stored.
    pub fn count(&self) -> io::Result<usize> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let tbl = txn.open_table(SCORES).map_err(io::Error::other)?;
        let mut n = 0usize;
        for entry in tbl.iter().map_err(io::Error::other)? {
            entry.map_err(io::Error::other)?;
            n += 1;
        }
        Ok(n)
    }
}

fn parse_score(json: &str) -> io::Result<Score> {
    serde_json::from_str(json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn sort_newest_first(scores: &mut [Score]) {
    scores.sort_by(|a, b| b.ts_unix_nano.cmp(&a.ts_unix_nano));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, ScoreSource, ScoreTarget};

    fn score(id: &str, target: ScoreTarget, ts: u64) -> Score {
        Score {
            id: id.to_string(),
            target,
            name: "exact_match".to_string(),
            num_value: Some(1.0),
            str_value: None,
            data_type: DataType::Numeric,
            source: ScoreSource::Eval,
            comment: None,
            config_id: None,
            agg_stats: None,
            ts_unix_nano: ts,
        }
    }

    #[test]
    fn put_get_by_target_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScoreStore::open(&dir.path().join("scores.redb")).unwrap();
        store
            .put_batch(&[
                score("s1", ScoreTarget::Span("aa".into()), 10),
                score("s2", ScoreTarget::Span("aa".into()), 30),
                score("s3", ScoreTarget::Trace("tt".into()), 20),
            ])
            .unwrap();

        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(store.get("s2").unwrap().unwrap().name, "exact_match");
        assert!(store.get("nope").unwrap().is_none());

        let by_span = store.by_target_key("span:aa").unwrap();
        assert_eq!(by_span.len(), 2);
        assert_eq!(by_span[0].id, "s2"); // newest first
        assert_eq!(store.by_target_key("trace:tt").unwrap().len(), 1);
        assert!(store.by_target_key("span:zz").unwrap().is_empty());

        let recent = store.list(10).unwrap();
        assert_eq!(
            recent.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s2", "s3", "s1"]
        );
    }

    #[test]
    fn upsert_overwrites_without_duplicating_target_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScoreStore::open(&dir.path().join("scores.redb")).unwrap();
        store
            .put_batch(&[score("s1", ScoreTarget::Span("aa".into()), 10)])
            .unwrap();
        let mut updated = score("s1", ScoreTarget::Span("aa".into()), 99);
        updated.comment = Some("revised".into());
        store.put_batch(&[updated]).unwrap();

        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(store.by_target_key("span:aa").unwrap().len(), 1);
        assert_eq!(
            store.get("s1").unwrap().unwrap().comment.as_deref(),
            Some("revised")
        );
    }

    #[test]
    fn upsert_with_changed_target_clears_stale_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScoreStore::open(&dir.path().join("scores.redb")).unwrap();
        store
            .put_batch(&[score("s1", ScoreTarget::Span("aa".into()), 10)])
            .unwrap();
        assert_eq!(store.by_target_key("span:aa").unwrap().len(), 1);

        // Re-put the same id under a different target.
        store
            .put_batch(&[score("s1", ScoreTarget::Trace("tt".into()), 20)])
            .unwrap();

        assert!(
            store.by_target_key("span:aa").unwrap().is_empty(),
            "stale span mapping must be cleared"
        );
        assert_eq!(store.by_target_key("trace:tt").unwrap().len(), 1);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scores.redb");
        {
            let store = ScoreStore::open(&path).unwrap();
            store
                .put_batch(&[score("s1", ScoreTarget::Span("aa".into()), 10)])
                .unwrap();
        }
        let store = ScoreStore::open(&path).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(store.by_target_key("span:aa").unwrap().len(), 1);
    }
}
