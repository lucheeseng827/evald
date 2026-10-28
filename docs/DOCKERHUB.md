# evald (`mancube/evald`)

**An OTel-native trace + eval store in one static binary.** Point your OpenInference/OTel exporter at `:4318`, see your traces, run evals from a JSONL dataset, get scores keyed to the exact span — and gate CI on regressions. No database, no Python runtime: a single static musl binary that runs on your laptop, inside a locked-down CI runner, or on an air-gapped host.

- **Image:** `mancube/evald` — static musl binary on **distroless/static**, runs as **nonroot** (uid `65532`), no shell, no package manager.
- **Arch:** `linux/amd64`, `linux/arm64` · **Binary inside:** `/usr/local/bin/evald` (entrypoint) · **Exposes:** `4318` (OTLP/HTTP receiver + query/SQL API + embedded UI)
- **Build:** the **default** feature set — pure Rust, zero Python/C/C++. DataFusion/Arrow/Parquet, redb, and the axum stack are all pure-Rust, and the embedded SPA is baked in (no Node).
- **Source / full docs:** [github.com/lucheeseng827/evald](https://github.com/lucheeseng827/evald) · Apache-2.0

## Where it fits

`evald` is the **trace-ingest + eval store** that sits between the apps emitting
LLM/agent telemetry and the humans (and CI) reading it. Point any OpenInference/OTel
exporter at `:4318`; it normalizes each span, keeps it in a durable local store, and
serves the UI, SQL, and eval/CI surfaces off the same data. No collector, no external
database, no Python.

```
   SOURCES                     EVALD                    STORE            CONSUMERS
   (emit OTLP spans)           (this image)             (local, durable) (read + gate)

 ┌──────────────┐
 │ LLM app      │  OTel SDK ─┐
 │ (SDK export) │            │
 └──────────────┘            │
 ┌──────────────┐            │      ┌─────────────┐    ┌──────────────────┐
 │ Agent /      │  spans ────┼────▶ │  WAL (redb) │    │ Embedded UI      │
 │ framework    │  traces    │      │  (durable)  │──▶ │ (trace → tree →  │
 └──────────────┘            │      └──────┬──────┘    │  scores)         │
 ┌──────────────┐    ┌───────┴──────┐      │           └──────────────────┘
 │ OTel         │ ─▶ │    evald     │ ◀────┘    ┌────▶ │ SQL / query API  │
 │ Collector    │    │ ingest·norm  │      ┌─────────┐ │ (DataFusion)     │
 └──────────────┘    │ ·store·eval  │─────▶│ Parquet │ └──────────────────┘
                     └──────┬───────┘      │ blocks  │ ┌──────────────────┐
   POST /v1/traces          │  scores ────▶│ ∪ scores│─│ eval run/compare │
   (protobuf · OTLP-JSON)   │              └─────────┘ │ (CI regression   │
                            └──────────────────────────│  gate, exit ≠ 0) │
                                                        └──────────────────┘
```

- **Upstream** — anything that speaks OTLP/OpenInference: an app's OTel SDK, an agent
  framework's tracer, or an OTel Collector fanning out. `evald` *is* the receiver
  (`POST /v1/traces`, protobuf or OTLP-JSON) — no separate collector required.
- **evald** — normalizes each span into one model, writes it durably (WAL → Parquet),
  attaches scores keyed to the exact span, and answers queries.
- **Downstream** — the embedded UI (trace list → tree → scores), read-only DataFusion
  SQL over `spans` ∪ `scores`, and offline `eval run`/`eval compare` that gate CI on a
  regression.

## Tags

| Tag | Notes |
|---|---|
| `latest` | newest stable release |
| `0.3.0` | derived `cost_usd` from model + token counts (built-in price table), LLM usage on `/metrics` + `evald latency`, quieter default ingest logging, lower `--max-hot-spans` default, bounded SQL/hot-tier reads that stop over-allocating |
| `0.2.0` | redesigned console (Vite + React + TypeScript), standalone `mancube/evald-console` frontend image, `docs/INSTRUMENTATION.md`, `GET /v1/meta` |
| `0.1.0` | first release — OTLP ingest + normalization, durable WAL→Parquet store, scores, offline `eval run`/`eval compare` with CI gating, SQL, embedded UI |
| `*-rc.*` | pre-release smoke builds (not tagged `latest`) — don't use in production |

Pin a version in production: `mancube/evald:<version>`. From 0.4.0 every release is pushed as
both `0.4.0` and `v0.4.0`, and the image is cosign-signed with its SBOM attested
(`docs/OPERATIONS.md` § Verify what you pulled).

## Quick start

The binary is the entrypoint, so the `docker` args are just `evald` subcommands (`serve` / `eval` / `query` / `version`). Mount a data dir for the durable store; everything stays local.

**Create the data dir first, and run as its owner** (`--user`). The image runs as uid `65532`, which owns nothing on your machine or a CI runner, so it cannot create its store in a directory you mount: `serve` and `eval run` stop with `Permission denied (os error 13)`. And a bind-mounted directory that does not exist yet is created by Docker, as root, which `--user` alone does not fix — hence the `mkdir`.

```bash
docker run --rm mancube/evald version
```

**1. Run the store** — OTLP/HTTP (and the UI) on 4318, OTLP/gRPC on 4317. From 0.4.0 that is what
the image does with no command: it serves on both ports, inside the container on every interface,
into the `/data` volume. A named volume needs no `--user`: `/data` belongs to the image's user.

```bash
docker run -d --name evald -p 127.0.0.1:4318:4318 -p 127.0.0.1:4317:4317 \
  -v evald-data:/data mancube/evald
# point your app's OTel SDK at http://127.0.0.1:4318 (or `evald connect otel-app`),
# then open http://127.0.0.1:4318/. `docker stop evald` drains cleanly.
```

With a directory of your own instead, create it and run as its owner:

```bash
mkdir -p evald-data
docker run -d -p 127.0.0.1:4318:4318 --user "$(id -u):$(id -g)" \
  -v "$PWD/evald-data:/data" mancube/evald
```

Up to 0.3.x the image printed its version by default; there, pass
`serve --otlp-http 0.0.0.0:4318 --data-dir /data` explicitly.

**2. Gate CI on an offline eval** — score a JSONL dataset and exit non-zero on a regression:

```bash
docker run --rm --user "$(id -u):$(id -g)" \
  -v "$PWD:/work" -w /work \
  mancube/evald \
  eval run --config eval.yaml --data-dir /work/evald-data
# then diff two runs, failing the build only on a statistically significant drop:
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work mancube/evald \
  eval compare <run_a> <run_b> --data-dir /work/evald-data \
  --fail-on-regression --significance
```

**3. Query the store with SQL** — DataFusion over the Parquet blocks ∪ scores:

```bash
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD/evald-data:/data" mancube/evald \
  query "SELECT model, COUNT(*) n, SUM(total_tokens) tok FROM spans GROUP BY model" \
  --data-dir /data
```

> **Permissions:** the image runs as nonroot (uid `65532`), so the mounted `/data` dir must be writable by the user it runs as. The commands above use `--user "$(id -u):$(id -g)"` on a directory you created; on a host where it must stay uid 65532 (a shared volume, say), `chown 65532:65532 evald-data` instead. Avoid `chmod 777`: it makes the trace store world-writable.

## Commands

| Command | What it does |
|---|---|
| *(no command)* / `serve` | OTLP/HTTP + OTLP/gRPC receiver, durable store, query/SQL API, embedded UI (`/data`, both ports) |
| `connect <tool>` | print the configuration that points `claude-code`, `codex`, `gemini-cli`, an `otel-app` or a `collector` at this evald |
| `sessions --data-dir <dir>` | agent sessions: calls, tool calls, failures, cost |
| `eval run --config <eval.yaml> --data-dir <dir>` | score a JSONL dataset; exit non-zero on a threshold regression (CI gate) |
| `eval compare <runA> <runB> [--fail-on-regression] [--significance]` | diff two runs by evaluator; gate on a (statistically significant) regression |
| `query "<SQL>" --data-dir <dir>` | read-only DataFusion SQL over `spans` ∪ `scores` |
| `version` | print the version |

## Air-gap friendly

The default build (this image) makes **zero network calls** — ingest, storage, query, and the deterministic Tier-1 evaluators are entirely local and offline. It runs disconnected, on a laptop, in a locked-down CI runner, or on an air-gapped host. distroless/static + nonroot keeps the attack surface to just the binary. (Optional LLM-as-judge / embedding evaluators are BYO-key and call out only when you opt in.)

## License

Apache-2.0.
