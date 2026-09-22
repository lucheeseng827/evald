//! Commit index — redb (PoC step 4).
//!
//! redb is the atomicity point of the hot→cold commit protocol (PLAN.md §1.3 step 2):
//! a single ACID write transaction advances the **watermark** (the highest WAL segment
//! flushed to cold) *and* records the new blocks (and their `trace_id`s) together. The
//! watermark is what recovery trusts; the block table is the *source of truth* for which
//! Parquet files are committed, so an orphan block from a crashed flush (written but
//! never committed) is simply never read.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use redb::{
    Database, MultimapTableDefinition, ReadableDatabase, ReadableMultimapTable, ReadableTable,
    ReadableTableMetadata, TableDefinition,
};

use super::cold::BlockMeta;

// key "watermark" -> highest WAL seqno fully flushed to cold.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
// block rel_path -> max_start_unix_nano (enumerable; the set of committed blocks).
const BLOCKS: TableDefinition<&str, u64> = TableDefinition::new("blocks");
// trace_id -> { block rel_path } (read only the blocks that can hold a given trace).
const TRACE_BLOCKS: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("trace_blocks");

const WATERMARK_KEY: &str = "watermark";

pub struct Index {
    db: Database,
}

impl Index {
    /// Open (or create) the commit index at `path`, materializing its tables.
    pub fn open(path: &Path) -> io::Result<Index> {
        let db = Database::create(path).map_err(io::Error::other)?;
        // Materialize the tables so first-run reads don't error on a missing table.
        let txn = db.begin_write().map_err(io::Error::other)?;
        {
            txn.open_table(META).map_err(io::Error::other)?;
            txn.open_table(BLOCKS).map_err(io::Error::other)?;
            txn.open_multimap_table(TRACE_BLOCKS)
                .map_err(io::Error::other)?;
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(Index { db })
    }

    /// The highest WAL seqno durably flushed to cold (0 if none).
    pub fn watermark(&self) -> io::Result<u64> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let table = txn.open_table(META).map_err(io::Error::other)?;
        let wm = table
            .get(WATERMARK_KEY)
            .map_err(io::Error::other)?
            .map(|v| v.value())
            .unwrap_or(0);
        Ok(wm)
    }

    /// The commit: record `blocks` (+ their trace mappings) and advance the watermark to
    /// `sealed_seqno`, atomically and durably. After this returns, the flush is committed.
    pub fn commit_flush(&self, sealed_seqno: u64, blocks: &[BlockMeta]) -> io::Result<()> {
        let txn = self.db.begin_write().map_err(io::Error::other)?;
        {
            let mut meta = txn.open_table(META).map_err(io::Error::other)?;
            let mut block_tbl = txn.open_table(BLOCKS).map_err(io::Error::other)?;
            let mut trace_tbl = txn
                .open_multimap_table(TRACE_BLOCKS)
                .map_err(io::Error::other)?;
            for b in blocks {
                block_tbl
                    .insert(b.rel_path.as_str(), b.max_start_unix_nano)
                    .map_err(io::Error::other)?;
                for trace_id in &b.trace_ids {
                    trace_tbl
                        .insert(trace_id.as_str(), b.rel_path.as_str())
                        .map_err(io::Error::other)?;
                }
            }
            meta.insert(WATERMARK_KEY, sealed_seqno)
                .map_err(io::Error::other)?;
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(())
    }

    /// The merge commit: record `output` (+ its trace mappings) and remove every input block
    /// with exactly the `trace_id → input` mappings it held, in one atomic transaction. The
    /// watermark is untouched — a merge moves spans between cold files, it flushes nothing.
    ///
    /// `inputs` carries each input's own trace set, collected while its rows were read for
    /// the merge, so the removal is `O(rows moved)` rather than a scan of every trace the
    /// store has ever seen (which is what [`Index::drop_blocks`] must do, knowing nothing).
    /// After this returns the inputs are unreferenced: readers no longer open them, and the
    /// aged orphan sweep may unlink them.
    pub fn commit_merge(
        &self,
        output: &BlockMeta,
        inputs: &[(String, BTreeSet<String>)],
    ) -> io::Result<()> {
        let txn = self.db.begin_write().map_err(io::Error::other)?;
        {
            let mut block_tbl = txn.open_table(BLOCKS).map_err(io::Error::other)?;
            let mut trace_tbl = txn
                .open_multimap_table(TRACE_BLOCKS)
                .map_err(io::Error::other)?;
            for (rel, traces) in inputs {
                block_tbl.remove(rel.as_str()).map_err(io::Error::other)?;
                for trace_id in traces {
                    trace_tbl
                        .remove(trace_id.as_str(), rel.as_str())
                        .map_err(io::Error::other)?;
                }
            }
            block_tbl
                .insert(output.rel_path.as_str(), output.max_start_unix_nano)
                .map_err(io::Error::other)?;
            for trace_id in &output.trace_ids {
                trace_tbl
                    .insert(trace_id.as_str(), output.rel_path.as_str())
                    .map_err(io::Error::other)?;
            }
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(())
    }

    /// How many blocks the index holds — the number the cold tier's read path opens.
    pub fn block_count(&self) -> io::Result<u64> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let table = txn.open_table(BLOCKS).map_err(io::Error::other)?;
        table.len().map_err(io::Error::other)
    }

    /// Every committed block path (the orphan-sweep allowlist and the no-filter scan set).
    pub fn all_block_paths(&self) -> io::Result<Vec<String>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let table = txn.open_table(BLOCKS).map_err(io::Error::other)?;
        let mut paths = Vec::new();
        for entry in table.iter().map_err(io::Error::other)? {
            let (path, _max_start) = entry.map_err(io::Error::other)?;
            paths.push(path.value().to_string());
        }
        Ok(paths)
    }

    /// The committed blocks that may contain spans for `trace_id`.
    pub fn block_paths_for_trace(&self, trace_id: &str) -> io::Result<Vec<String>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let table = txn
            .open_multimap_table(TRACE_BLOCKS)
            .map_err(io::Error::other)?;
        let mut paths = Vec::new();
        for entry in table.get(trace_id).map_err(io::Error::other)? {
            paths.push(entry.map_err(io::Error::other)?.value().to_string());
        }
        Ok(paths)
    }

    /// Every committed block with its `max_start_unix_nano` — the retention scan input. A block
    /// whose newest span predates the cutoff can be dropped whole (see [`Store::reclaim_before`]).
    pub fn blocks_with_max_start(&self) -> io::Result<Vec<(String, u64)>> {
        let txn = self.db.begin_read().map_err(io::Error::other)?;
        let table = txn.open_table(BLOCKS).map_err(io::Error::other)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(io::Error::other)? {
            let (path, max_start) = entry.map_err(io::Error::other)?;
            out.push((path.value().to_string(), max_start.value()));
        }
        Ok(out)
    }

    /// Remove `paths` from the block table **and** every `trace_id → block` mapping pointing at
    /// them, in one atomic transaction. The Parquet files themselves are unlinked by the caller;
    /// doing this index removal **first** is crash-safe, since a file left on disk without an index
    /// entry is an orphan that the open-time sweep reclaims. A no-op for an empty `paths`.
    pub fn drop_blocks(&self, paths: &[String]) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let dropping: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
        let txn = self.db.begin_write().map_err(io::Error::other)?;
        {
            let mut block_tbl = txn.open_table(BLOCKS).map_err(io::Error::other)?;
            for p in paths {
                block_tbl.remove(p.as_str()).map_err(io::Error::other)?;
            }
            let mut trace_tbl = txn
                .open_multimap_table(TRACE_BLOCKS)
                .map_err(io::Error::other)?;
            // Collect the (trace_id, block) mappings that point at a dropped block first — the
            // iterator borrows the table, so removals happen only after it is fully consumed.
            let mut stale: Vec<(String, String)> = Vec::new();
            for entry in trace_tbl.iter().map_err(io::Error::other)? {
                let (key, values) = entry.map_err(io::Error::other)?;
                let trace_id = key.value().to_string();
                for v in values {
                    let block = v.map_err(io::Error::other)?.value().to_string();
                    if dropping.contains(block.as_str()) {
                        stale.push((trace_id.clone(), block));
                    }
                }
            }
            for (trace_id, block) in stale {
                trace_tbl
                    .remove(trace_id.as_str(), block.as_str())
                    .map_err(io::Error::other)?;
            }
        }
        txn.commit().map_err(io::Error::other)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn block(path: &str, max_start: u64, traces: &[&str]) -> BlockMeta {
        BlockMeta {
            rel_path: path.to_string(),
            max_start_unix_nano: max_start,
            trace_ids: traces
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
        }
    }

    #[test]
    fn watermark_defaults_to_zero_then_advances() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.redb")).unwrap();
        assert_eq!(index.watermark().unwrap(), 0);

        index
            .commit_flush(3, &[block("blocks/a.parquet", 10, &["t1", "t2"])])
            .unwrap();
        assert_eq!(index.watermark().unwrap(), 3);

        index
            .commit_flush(4, &[block("blocks/b.parquet", 20, &["t2"])])
            .unwrap();
        assert_eq!(index.watermark().unwrap(), 4);

        let mut all = index.all_block_paths().unwrap();
        all.sort();
        assert_eq!(all, vec!["blocks/a.parquet", "blocks/b.parquet"]);

        let mut t2 = index.block_paths_for_trace("t2").unwrap();
        t2.sort();
        assert_eq!(t2, vec!["blocks/a.parquet", "blocks/b.parquet"]);
        assert_eq!(
            index.block_paths_for_trace("t1").unwrap(),
            vec!["blocks/a.parquet"]
        );
        assert!(index.block_paths_for_trace("nope").unwrap().is_empty());
    }

    #[test]
    fn drop_blocks_removes_block_and_its_trace_mappings() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.redb")).unwrap();
        index
            .commit_flush(1, &[block("blocks/a.parquet", 10, &["t1", "t2"])])
            .unwrap();
        index
            .commit_flush(2, &[block("blocks/b.parquet", 20, &["t2", "t3"])])
            .unwrap();

        let mut pairs = index.blocks_with_max_start().unwrap();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("blocks/a.parquet".to_string(), 10),
                ("blocks/b.parquet".to_string(), 20)
            ]
        );

        index
            .drop_blocks(&["blocks/a.parquet".to_string()])
            .unwrap();
        assert_eq!(index.all_block_paths().unwrap(), vec!["blocks/b.parquet"]);
        // t1 pointed only at a → now empty; t2 pointed at a AND b → now only b; t3 → b.
        assert!(index.block_paths_for_trace("t1").unwrap().is_empty());
        assert_eq!(
            index.block_paths_for_trace("t2").unwrap(),
            vec!["blocks/b.parquet"]
        );
        assert_eq!(
            index.block_paths_for_trace("t3").unwrap(),
            vec!["blocks/b.parquet"]
        );
        // A drop does not touch the watermark, and dropping nothing is a no-op.
        assert_eq!(index.watermark().unwrap(), 2);
        index.drop_blocks(&[]).unwrap();
        assert_eq!(index.all_block_paths().unwrap().len(), 1);
    }

    #[test]
    fn commit_merge_swaps_inputs_for_the_output_and_leaves_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let idx = Index::open(&dir.path().join("index.redb")).unwrap();
        idx.commit_flush(
            2,
            &[
                block("blocks/2026/01/01/00/a.parquet", 10, &["t1", "t2"]),
                block("blocks/2026/01/01/00/b.parquet", 20, &["t2", "t3"]),
            ],
        )
        .unwrap();
        idx.commit_flush(3, &[block("blocks/2026/01/01/01/c.parquet", 30, &["t4"])])
            .unwrap();

        let merged = block(
            "blocks/2026/01/01/00/merged-1-2.parquet",
            20,
            &["t1", "t2", "t3"],
        );
        let inputs = vec![
            (
                "blocks/2026/01/01/00/a.parquet".to_string(),
                ["t1", "t2"].iter().map(|s| s.to_string()).collect(),
            ),
            (
                "blocks/2026/01/01/00/b.parquet".to_string(),
                ["t2", "t3"].iter().map(|s| s.to_string()).collect(),
            ),
        ];
        idx.commit_merge(&merged, &inputs).unwrap();

        let mut paths = idx.all_block_paths().unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "blocks/2026/01/01/00/merged-1-2.parquet",
                "blocks/2026/01/01/01/c.parquet"
            ]
        );
        assert_eq!(idx.block_count().unwrap(), 2);
        assert_eq!(
            idx.watermark().unwrap(),
            3,
            "a merge never moves the watermark"
        );
        // t2 was in both inputs: exactly one mapping now, to the merged block.
        assert_eq!(
            idx.block_paths_for_trace("t2").unwrap(),
            vec!["blocks/2026/01/01/00/merged-1-2.parquet"]
        );
        assert_eq!(
            idx.block_paths_for_trace("t4").unwrap(),
            vec!["blocks/2026/01/01/01/c.parquet"]
        );
        assert!(idx.block_paths_for_trace("t1").unwrap().len() == 1);
    }

    #[test]
    fn reopen_persists_watermark_and_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        {
            let index = Index::open(&path).unwrap();
            index
                .commit_flush(9, &[block("blocks/x.parquet", 5, &["tt"])])
                .unwrap();
        }
        let index = Index::open(&path).unwrap();
        assert_eq!(index.watermark().unwrap(), 9);
        assert_eq!(
            index.block_paths_for_trace("tt").unwrap(),
            vec!["blocks/x.parquet"]
        );
    }
}
