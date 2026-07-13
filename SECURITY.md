# Security Policy

## Reporting a vulnerability

**Do not open a public issue for security problems.**

Report privately via GitHub's **Security Advisories** ("Report a vulnerability"
on the repository's Security tab), or email the maintainer listed on the GitHub
profile. Include: affected version/commit, a description, and a minimal
reproduction if possible.

We aim to acknowledge within **3 business days** and to ship a fix or mitigation
for confirmed, in-scope issues as soon as practical. We will credit reporters who
want it once a fix is released.

## Supported versions

The latest released minor version receives security fixes. Pre-1.0, older
versions may be patched only at the maintainer's discretion.

## Scope

evald is a single static binary that ingests **untrusted OTLP spans** and runs
**read-only SQL** over the stored data. In scope:

- The **OTLP/HTTP receiver** (`POST /v1/traces`) and its decode path: protobuf,
  OTLP-JSON, and gzip decompression of attacker-controlled bodies. Decompression
  bombs, malformed protobuf, and oversized payloads must be bounded, not crash the
  process or exhaust memory — the receiver caps decompressed size and sheds with
  `429/503 + Retry-After` rather than failing open.
- The **WAL / store recovery path**: a torn or corrupt WAL tail, or an orphan
  Parquet block from a crashed flush, must be detected (length + CRC32) and
  recovered without data corruption or double-count — never a panic-loop on open.
- The **SQL endpoint** (`POST /v1/sql`) and `evald query`: only read statements
  (`SELECT`/`WITH`/`EXPLAIN`) are accepted; the write path stays the commit
  protocol, never SQL. Report any way to mutate state or read outside `--data-dir`
  through SQL.
- The **scores / annotations endpoints** (`POST /v1/scores`,
  `POST /v1/span_annotations`): untrusted JSON bodies and the upsert-by-identifier
  path.
- **Network exposure defaults**: `evald serve` binds `127.0.0.1` by default and
  must never listen on a public interface unasked.
- The **optional bearer-token gate** (`--auth-token` / `EVALD_AUTH_TOKEN` /
  `--auth-token-file`): when armed, a way to reach any endpoint (HTTP or gRPC)
  *without* a valid token — a bypass, or a timing oracle that leaks the secret —
  is in scope. So is a token being written to a log or error.

Out of scope:

- **Running evald exposed with no protection at all.** The OSS core is a local /
  single-tenant tool; it now ships an *optional* bearer-token gate (off by
  default — see above), but running it on a shared or public network with
  *neither* that gate nor an authenticating reverse proxy / network policy in
  front is a deployment choice, not a vulnerability. The gate is a shared-secret
  bearer check, **not** TLS and **not** per-user identity — for TLS termination or
  per-tenant identity put a reverse proxy / gateway (or the separately-licensed ee
  fleet layer) in front.
- The **content of the spans and scores** evald stores is product data, not a
  vulnerability in evald (it is an observability store — it records what your app
  emits, including any secrets your app puts in span attributes; redact at the
  source).
- Third-party crates (DataFusion, Arrow, redb, etc.) — report those upstream;
  we will track and bump affected pins.

## Handling of secrets

evald stores no credentials and makes no outbound calls in the default build:
ingest, storage, query, and Tier-1 evaluation are entirely local and offline.
The optional Tier-2/Tier-3 evaluators (semantic similarity, LLM-as-judge) are
**BYO-key** and read the key from the environment at run time only — never logged,
never persisted to `--data-dir`.
