# evald operations runbook

Deploying, backing up, upgrading, monitoring, and debugging a running evald.
evald is a **single static binary + one local `--data-dir`** — there is no
database, container, or sidecar to operate. Durability contract, stated
honestly: **accepted spans are durable (fsynced before the ACK); under overload
evald sheds with explicit `429 + Retry-After` — it never silently drops.**
Internals (commit protocol, data model) live in [`PLAN.md`](../PLAN.md);
measured capacity numbers live in [`BENCHMARKS.md`](./BENCHMARKS.md).

## What the state is — `--data-dir` layout

Everything evald persists lives under one directory (default `evald-data`,
see [CONFIG.md](./CONFIG.md)):

```text
evald-data/
├── wal/                      # segmented write-ahead log — the ACK boundary
│   └── 00000000000000000042.wal    #   [u32 len][u32 crc32][JSON] frames; sealed segs await compaction
├── blocks/                   # cold tier — plain Snappy Parquet, time-partitioned
│   └── 2026/07/07/17/*.parquet     #   open format: DuckDB / pandas read these directly
├── index.redb                # trace_id → block index + the compaction watermark
├── scores.redb               # the universal Score store (evals, human annotations, API)
└── judge_cache.redb          # (only with LLM-judge use) cached judge results — scores only, never inputs/keys
```

The write path: OTLP request → normalize → bounded channel → single writer task
appends + fsyncs the WAL (**then** the client is ACKed) → hot tier (in memory)
→ background compactor flushes sealed segments to Parquet, records blocks +
advances the watermark in one redb transaction, then deletes the WAL segment.
Reads always see hot ∪ cold, deduped.

### Crash recovery semantics

On every open (`serve`, `eval`, `query`, `cost` all open the same store):

1. WAL segments **at or below** the index watermark are already in cold —
   deleted.
2. Segments **above** the watermark are replayed into the hot tier — nothing
   ACKed is lost, nothing is double-counted (the watermark only advances after a
   block is fsynced + renamed into place).
3. Orphan Parquet files not referenced by the index (a flush that crashed before
   its redb commit) and leftover `*.tmp` are swept — the index is the source of
   truth for committed blocks.

This is exercised by a real `kill -9` during compaction in
[`tests/`](../tests) and was re-verified by hand (kill the server mid-run;
reopen; the spans come back). A torn final WAL frame (crash mid-append) stops
replay at the last intact record — that final, never-ACKed batch is the only
thing a crash can lose.

## Deploy

Copy one binary; give it a directory:

```bash
evald serve --data-dir /var/lib/evald            # loopback :4318
```

- Run it under any supervisor (systemd, container, CI step). Ctrl-C / SIGINT
  triggers a graceful drain.
- **One process per data-dir.** The redb lock is exclusive — a second process
  (including `evald eval run` / `query` / `cost`) opening the same dir fails
  with `Database already open. Cannot acquire lock.` Point CLI runs at their own
  `--data-dir`, or stop the server first.
- Container images: see [`docs/DOCKERHUB.md`](./DOCKERHUB.md) and the
  [`Dockerfile`](../Dockerfile)s.

## Backup & restore

The state to snapshot is the **whole `--data-dir`** (WAL + blocks + the redb
files together — they are one consistent unit tied by the watermark).

Safe backup, in preference order:

1. **Cold backup** (simplest, always consistent): stop the process, copy the
   directory, restart.
2. **Filesystem/volume snapshot** (LVM, ZFS, EBS) while running: the commit
   protocol is crash-safe, so restoring a point-in-time snapshot behaves exactly
   like recovering from a power cut — ACKed spans up to the snapshot are
   present.
3. **Do not** `rsync`/copy the live directory file-by-file while the server is
   writing: the WAL, blocks, and index would be captured at different moments;
   an index that references a block the copy missed makes reads on the restored
   dir fail.

Restore = put the directory back and start evald against it. Recovery runs
automatically on open.

Partial-loss note: the Parquet blocks are plain files — even with a damaged
`index.redb`, the span **data** in `blocks/` remains readable by DuckDB/pandas.
Scores live only in `scores.redb`.

## Upgrade

PoC status: there is **no on-disk `format_version` yet and no cross-version
promise** (the WAL format is explicitly provisional — see
[CONFIG.md § Data-format & compatibility](./CONFIG.md#data-format--compatibility)).
The safe procedure:

1. Stop evald (graceful — lets in-flight WAL appends finish).
2. Back up the data-dir (above).
3. Swap the binary, start, and check the startup log line
   (`store opened spans_recovered=… watermark=…`) plus a `GET /v1/spans` smoke
   read.
4. Rollback = restore the old binary + the backup.

## Monitoring

evald logs structured `tracing` lines to stderr; there is no metrics endpoint
yet (label: planned). What to watch:

| Signal | How | Alert on |
|---|---|---|
| liveness | `curl -fs localhost:4318/v1/spans?limit=1` (add `-H "Authorization: Bearer …"` when the auth gate is on) | non-200 |
| backpressure / shed | client-side 429 rate; server log `shedding: ingest channel full` | any sustained 429s — ingest is outrunning WAL fsync throughput |
| store write failures | log `store write failed` (clients see 503) | any occurrence — usually disk full / permissions |
| compaction health | `wal/` directory: count + total size of `*.wal` | monotonic growth — compaction is lagging or disabled (`--compact-interval-secs 0`) |
| disk | free space on the data-dir volume | WAL + blocks grow with ingest; there is **no retention/TTL yet** (planned) — reclaiming space means deleting old `blocks/YYYY/MM/DD` partitions with the server stopped |
| recovery cost | startup log `spans_recovered=N` | large N with slow starts — lower `--seal-threshold` so less lives in the WAL |

Capacity guidance (measured, same-box: see [BENCHMARKS.md](./BENCHMARKS.md)):
~56k spans/s fsynced-before-ACK at 32 connections on ~1.8 cores, 8.5 MB idle
RSS. SQL scans run off the ingest hot path but do burn CPU — schedule heavy
analytics accordingly.

## Troubleshooting (symptom → cause → fix)

**`429 Too Many Requests` + `Retry-After: 1` (`evald: overloaded, retry shortly`)**
The bounded ingest channel (1024 batches) is full — WAL fsync throughput can't
keep up with the arrival rate. This is deliberate shedding, not data loss:
un-ACKed batches were never accepted. Fix: let the SDK's OTLP exporter retry
(it honors Retry-After), batch spans (100-span batches nearly double measured
throughput), put the data-dir on faster storage, or slow the producer.

**`413 Payload Too Large` on `POST /v1/traces`**
One OTLP batch decompressed past 16 MiB. Lower the SDK's
`max_export_batch_size`. The cap is on the *inflated* size, so a tiny gzip that
expands past the cap is also 413 — that is the decompression-bomb guard working.

**`503 Service Unavailable` (`evald: store unavailable`)**
The writer task could not append/fsync — almost always disk full, a permissions
change, or the data-dir volume disappearing. Check stderr for
`store write failed` and the underlying io error; free space and restart.

**`Error: opening store at …: Database already open. Cannot acquire lock.`**
Two processes on one data-dir (e.g. `evald eval run` while `evald serve` holds
the same `--data-dir`). Stop the server, or give the CLI its own dir. CI eval
runs are designed to use a standalone dir.

**WAL keeps growing / restart takes long (`spans_recovered` is huge)**
Compaction is lagging or off. Check `--compact-interval-secs` isn't `0`, check
stderr for `compaction pass failed (will retry)` (a failing flush retries on the
next tick and the WAL is *not* truncated — again usually disk), and consider a lower
`--seal-threshold` (a segment only becomes flushable once sealed, so a
50k-span threshold on a low-rate stream keeps spans in the WAL/hot tier for a
long time — reads are unaffected, restart replay just costs more).

**A span I POSTed doesn't show in `GET /v1/spans`**
Check the ingest response first: a `400` means the payload never decoded (evald
answers 400 with the decode error; note OTLP-JSON must use the protobuf JSON
field names, e.g. `traceId`, `startTimeUnixNano`). A `200` means it is stored —
`GET /v1/spans` defaults to the most recent 100, so pass `?trace_id=` or a
bigger `limit`.

**`401 Unauthorized` (`evald: unauthorized — set Authorization: Bearer <token>`)**
The bearer-token gate is armed (`--auth-token` / `EVALD_AUTH_TOKEN` /
`--auth-token-file`) and the request had no valid token. Add
`-H "Authorization: Bearer <token>"` (curl / probes), set
`OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token>"` (OTLP SDKs), or, for
gRPC, send the token as `authorization` metadata. A browser navigation can't send
the header — reach the SPA over a loopback tunnel or a header-injecting proxy. If
you did **not** mean to require auth, unset the flags/env — an unset or
present-but-empty `EVALD_AUTH_TOKEN=` / `EVALD_AUTH_TOKEN_FILE=` value leaves auth
**off** (it does not fail boot). If `serve` **exits at boot** with `auth
configuration error: …`, a token is under 16 chars or has a non-ASCII character, or a
`--auth-token-file` you named was unreadable or held no usable tokens — an
explicitly-named token file that is empty or comment-only is treated as a
misconfiguration and fails loud (rather than silently starting wide-open).

**`400 evald: SQL error: only read queries are allowed …`**
The `/v1/sql` guard rejected a non-read statement (`INSERT`, `COPY`,
`EXPLAIN ANALYZE`, multi-statement bodies). The endpoint is read-only by design;
writes go through OTLP/scores only.

**SQL results end at exactly 1 000 rows / `"truncated": true`**
The default result cap. Pass `{"limit": …}` (max 100 000) or aggregate in SQL
instead of pulling raw rows.

**`eval compare` says `no aggregate scores found for run …`**
Wrong run id, a different `--data-dir` than the one `eval run` wrote to, or the
run predates score persistence. `eval run` prints `run_id:` — compare exactly
those, with the same `--data-dir`.

**Judge eval fails with a missing-key error / makes no calls**
Tier-3 judges need a binary built with `--features judge` **and**
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` at run time (read at call time only).
Use `evald eval run --estimate` first — it needs neither. Re-runs hit
`judge_cache.redb` and are free.

## Security posture

- **Listens:** one TCP listener, `127.0.0.1:4318` by default
  (`--otlp-http` / `EVALD_OTLP_HTTP_ADDR`; the OTLP/gRPC `:4317` sibling is the
  same). Loopback-only unless you deliberately bind wider.
- **Authn/z: optional bearer-token gate, OFF by default.** With no token
  configured, anyone who can reach the port can write spans, write scores, and read
  everything (including `POST /v1/sql` full scans) — the intended threat model is
  untrusted *input* on a *trusted* network (laptop, locked-down CI runner). Arm the
  gate with `--auth-token <tok>` (repeatable), `EVALD_AUTH_TOKEN` (comma-separated),
  and/or `--auth-token-file <path>` (one token per line; blank lines and `#`
  comments ignored) to require an `Authorization: Bearer <token>` on **every** HTTP
  request (OTLP ingest, `/v1/*`, and the SPA) and **every** OTLP/gRPC request (the
  `authorization` metadata). A missing/wrong token is `401` (HTTP) /
  `UNAUTHENTICATED` (gRPC). Tokens must be **≥16 printable-ASCII chars** (rejected
  at boot otherwise), are matched by SHA-256 digest, and are **never logged**. The
  three sources **union** (a `--auth-token` flag is one whole token, `EVALD_AUTH_TOKEN`
  is a comma-separated list, `--auth-token-file` is one per line), so you can rotate
  (add a new token, drain clients, remove the old) and revoke per client. A
  present-but-empty `EVALD_AUTH_TOKEN=` placeholder leaves auth **off** (it does not
  fail boot). It is a shared-secret gate, **not** per-user identity — every valid token
  has full access.
- **Startup tells you if you're exposed:** binding a listener off-loopback with no
  token logs a loud `WARN` (pointing here); with a token it logs `auth: … ARMED`
  (the token *count* only — never the tokens).
- **TLS:** evald does no TLS. A bearer token over plaintext HTTP is only as private
  as the network, so if the path from client to evald is untrusted, terminate TLS at
  a reverse proxy (nginx, Caddy, an mTLS mesh, or the sibling **edgeguard** front
  door) in front of `:4318`/`:4317` — even with the token gate on.
- **Exposing it — pick one:** (a) the built-in token gate, on a trusted-transport
  segment or behind a TLS-terminating load balancer; (b) a reverse proxy that
  terminates TLS *and* authenticates; (c) keep evald loopback and let the
  separately-licensed ee fleet layer handle multi-node ingest/auth. Caveat for the
  gate: a **browser** top-level navigation can't send an `Authorization` header, so
  reach the SPA over an SSH tunnel to loopback or via a proxy that injects the
  header; likewise an HTTP **liveness probe** must carry the token when the gate is
  on (`curl -H "Authorization: Bearer …" .../v1/spans?limit=1`).
- **Input hardening that is built in:** decompressed-size body cap (16 MiB, the
  gzip-bomb guard), parsed-statement read-only SQL gate, bounded ingest channel
  (429 shed), clamped read limits.
- **Secrets:** the server needs none. Judge API keys are read from the
  environment at call time by the `eval` CLI only — never logged, never
  persisted, never part of the cache key
  ([`src/judge.rs`](../src/judge.rs)).
- **Data at rest:** WAL/Parquet/redb are unencrypted local files — protect the
  data-dir with filesystem permissions/volume encryption. Span payloads can
  contain prompts/completions; treat the data-dir as sensitive.
- Vulnerability disclosure: [`SECURITY.md`](../SECURITY.md).
