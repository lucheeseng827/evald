# evald on-disk format

evald's state is a directory of **plain files** — a write-ahead log, Snappy Parquet
blocks, and two redb tables. Nothing here is a private encoding you need evald to
read: the spans are Parquet, and DuckDB, pandas or any Arrow reader opens them
directly. This page is the contract for that directory: what the files are, what
evald guarantees about them across releases, and what happens when the two do not
match.

**Current format version: 1.**

## The marker

Every data-dir carries a `FORMAT` file at its root:

```json
{
  "format": 1,
  "evald_version": "0.2.0",
  "created_unix_ms": 1789550360249
}
```

It is written when the directory is first used, and read on every open. A
directory stamped by a migration from a pre-marker release also carries
`"stamped_from": "legacy"`.

The marker is the only thing that has to be understood before anything else is
read, which is why it is one small JSON file rather than a header inside a
database.

## The compatibility policy

1. **A release reads its own format and the one before it.** Upgrading evald never
   requires exporting and re-importing. `evald migrate` reports what a data-dir
   needs and does it.
2. **A newer data-dir is refused, with an explanation.** If the directory says
   format 2 and the binary understands 1, evald exits rather than opening it
   optimistically:

   ```
   data-dir evald-data is on-disk format 2, written by evald 9.9.9;
   this evald (0.2.0) reads formats up to 1.
   Upgrade evald — a data-dir is never downgraded (docs/FORMAT.md).
   ```

   Opening it anyway is the failure mode this rule exists to prevent: an older
   binary that silently ignores what it does not recognise writes a directory only
   it can read.
3. **A directory with no marker predates the freeze.** It is stamped in place. No
   block is rewritten, nothing moves — the layout was already format 1; only the
   statement of it was missing.
4. **A format change is a version bump and a migration**, never a silent
   reinterpretation of existing bytes. `tests/format_freeze.rs` reads a committed
   format-1 directory on every CI run, so a change that breaks it fails the build
   rather than a user's upgrade.

## The layout

```text
evald-data/
├── FORMAT                    # the marker above
├── wal/                      # write-ahead log — the ACK boundary
│   └── 00000000000000000042.wal
├── blocks/                   # cold tier — Snappy Parquet, time-partitioned by span start (UTC)
│   ├── 2026/07/07/17/00000000000000000041-0.parquet     # a flush block
│   ├── 2026/07/07/17/merged-…-….parquet                 # an hour merge
│   └── 2026/07/07/day-…-….parquet                       # a day merge
├── blobs/                    # oversized payloads offloaded out of the spans
├── index.redb                # trace_id → block, and the compaction watermark
└── scores.redb               # the Score store
```

`index.redb` is the source of truth for which blocks are committed. A Parquet file
it does not name is not part of the store — it is either a crashed flush's orphan
or a merge input past its grace, and it is swept.

### WAL segments

`<seqno:020>.wal`, frames of `[u32 length][u32 crc32][JSON span batch]`. A segment
is sealed at the configured span threshold and deleted once its spans are durable
in a block. A torn final frame (a crash mid-append) stops replay at the last intact
record.

### Block files

Every block is Parquet with the same schema, whatever wrote it. The file name
records where its rows came from:

| Name | Written by | Covers |
|---|---|---|
| `<seqno:020>-<idx>.parquet` | a WAL flush | one sealed segment, one partition |
| `merged-<lo:020>-<hi:020>.parquet` | an hour merge | seqnos `lo..=hi` in that hour |
| `day-<lo:020>-<hi:020>.parquet` | a day merge | seqnos `lo..=hi` in that day |

Each block also carries key-value metadata in its Parquet footer, so a consumer
that only has the file knows what it is:

| Key | Meaning |
|---|---|
| `evald.format` | the on-disk format version the block was written at |
| `evald.block_kind` | `flush` or `merged` |
| `evald.seqno_lo` / `evald.seqno_hi` | the WAL seqno range the rows came from |
| `evald.merged_from` | JSON array of the block paths this one replaced (merged blocks only) |
| `evald.writer` | the evald version that wrote it |
| `evald.price_tables` | JSON array of the price-table versions whose derived costs the rows may hold: the table in force when a flush wrote it, the union of the inputs' for a merge. Since 0.3.0; absent before, and informational — nothing per span records its table, and `evald cost --price-table` re-prices from the stored counts |

`evald.merged_from` is what makes merging safe for anything mirroring the
directory: the fleet uploader reads it to retire the blocks a merge replaced,
instead of shipping both and double-counting the spans.

## Merged blocks

Every sealed WAL segment becomes a block, so without merging the block count only
grows — and so does the set of files a query has to open, until scans fail with
`Too many open files`. Cold-to-cold compaction bounds it:

- An hour partition's blocks merge once enough blocks **of a similar size** have
  accumulated (`--cold-merge-threshold`, default 16). Merging only within a size
  class is what keeps write amplification logarithmic: sixteen small blocks become
  one block several sizes up, which is not an input again until sixteen of *its*
  size exist. Merging everything on every pass would re-read the whole partition
  each time.
- A fully-past UTC day collapses into day blocks once nothing has been written
  into it for `--cold-merge-day-quiet-secs` (default an hour), so a low-volume
  store's block count grows per day rather than per hour, and a backfill does not
  have the day rewritten on every tick.
- No merged block exceeds `--cold-merge-max-spans` rows (default 1,000,000), so
  retention — which drops blocks whole — stays granular.

A merge is committed the way a flush is: the output is written to a temp file,
fsynced, renamed into place, and the directory fsynced; then **one** redb
transaction swaps the inputs for the output. Only after that commit is an input
unreferenced, and it is not deleted immediately — a query that listed it moments
earlier is still reading it, so it stays for `--cold-merge-grace-secs` (default
60) and is then reclaimed. A crash at any point leaves either the old blocks or
the new one indexed, never both, and the unreferenced file is swept.

`evald compact` runs the same merge on demand and collapses a partition fully,
ignoring the tick's size classes and quiet window.

## Reading the blocks without evald

The blocks are ordinary Parquet:

```sql
-- DuckDB, over a whole store
SELECT model, COUNT(*) FROM read_parquet('evald-data/blocks/**/*.parquet') GROUP BY model;
```

Two things to know. Rows are deduplicated by `(trace_id, span_id)` on evald's read
path, and blocks written across a crash window can briefly overlap — so add
`SELECT DISTINCT ON (trace_id, span_id)` (or `GROUP BY`) if exactness matters more
than speed. And spans still in the WAL are not in `blocks/` yet; `evald query`
unions them, a raw Parquet scan does not.

## Changing the format

If you are making a change that alters what is on disk:

1. Decide whether it is compatible. Adding a Parquet key-value key or a new
   optional JSON field is not a format change — readers ignore what they do not
   know. Renaming a column, changing a partition layout, or changing what a file
   name means is.
2. For a real change: bump `FORMAT_VERSION` in `src/store/format.rs`, write the
   migration, and regenerate the frozen fixture
   (`EVALD_REGEN_FIXTURE=1 cargo test --test format_freeze regenerate -- --ignored`)
   as a **separate, deliberate** commit — the old fixture is the evidence that the
   old format still opens.
3. Keep reading the previous version. The policy above is a promise about
   upgrades, not an aspiration.
