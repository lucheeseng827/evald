# evald (`mancube/evald`)

**An OTel-native trace + eval store in one static binary.** Point your OpenInference/OTel exporter at `:4318`, see your traces, run evals from a JSONL dataset, get scores keyed to the exact span — and gate CI on regressions. No database, no Python runtime: a single static musl binary that runs on your laptop, inside a locked-down CI runner, or on an air-gapped host.

- **Image:** `mancube/evald` — static musl binary on **distroless/static**, runs as **nonroot** (uid `65532`), no shell, no package manager.
- **Arch:** `linux/amd64`, `linux/arm64` · **Binary inside:** `/usr/local/bin/evald` (entrypoint) · **Exposes:** `4318` (OTLP/HTTP receiver + query/SQL API + embedded UI)
- **Build:** the **default** feature set — pure Rust, zero Python/C/C++. DataFusion/Arrow/Parquet, redb, and the axum stack are all pure-Rust, and the embedded SPA is baked in (no Node).
- **Source / full docs:** [github.com/lucheeseng827/evald](https://github.com/lucheeseng827/evald) · Apache-2.0

## Tags

| Tag | Notes |
|---|---|
| `latest` | newest stable release |
| `0.1.0` | first release — OTLP ingest + normalization, durable WAL→Parquet store, scores, offline `eval run`/`eval compare` with CI gating, SQL, embedded UI |
| `*-rc.*` | pre-release smoke builds (not tagged `latest`) — don't use in production |

Pin a version in production: `mancube/evald:0.1.0`.

## Quick start

The binary is the entrypoint, so the `docker` args are just `evald` subcommands (`serve` / `eval` / `query` / `version`). Mount a data dir for the durable store; everything stays local.

```bash
docker run --rm mancube/evald:0.1.0 version
```

**1. Run the store** — the OTLP/HTTP receiver + query API + embedded UI. Bind `0.0.0.0` inside the container (the default `127.0.0.1` isn't reachable from the host):

```bash
docker run --rm -p 4318:4318 \
  -v "$PWD/evald-data:/data" \
  mancube/evald:0.1.0 \
  serve --otlp-http 0.0.0.0:4318 --data-dir /data
# point your app's OTel SDK at http://127.0.0.1:4318, then open http://127.0.0.1:4318/
```

**2. Gate CI on an offline eval** — score a JSONL dataset and exit non-zero on a regression:

```bash
docker run --rm \
  -v "$PWD:/work" -w /work \
  mancube/evald:0.1.0 \
  eval run --config eval.yaml --data-dir /work/evald-data
# then diff two runs, failing the build only on a statistically significant drop:
docker run --rm -v "$PWD:/work" -w /work mancube/evald:0.1.0 \
  eval compare <run_a> <run_b> --data-dir /work/evald-data \
  --fail-on-regression --significance
```

**3. Query the store with SQL** — DataFusion over the Parquet blocks ∪ scores:

```bash
docker run --rm -v "$PWD/evald-data:/data" mancube/evald:0.1.0 \
  query "SELECT model, COUNT(*) n, SUM(total_tokens) tok FROM spans GROUP BY model" \
  --data-dir /data
```

> **Permissions:** the image runs as nonroot (uid `65532`), so the mounted `/data` dir must be writable by that uid — e.g. `mkdir -p evald-data && chmod 777 evald-data` (or `chown 65532:65532 evald-data`) before the first run.

## Commands

| Command | What it does |
|---|---|
| `serve --otlp-http 0.0.0.0:4318 --data-dir <dir>` | OTLP/HTTP receiver + durable store + query/SQL API + embedded UI |
| `eval run --config <eval.yaml> --data-dir <dir>` | score a JSONL dataset; exit non-zero on a threshold regression (CI gate) |
| `eval compare <runA> <runB> [--fail-on-regression] [--significance]` | diff two runs by evaluator; gate on a (statistically significant) regression |
| `query "<SQL>" --data-dir <dir>` | read-only DataFusion SQL over `spans` ∪ `scores` |
| `version` | print the version |

## Air-gap friendly

The default build (this image) makes **zero network calls** — ingest, storage, query, and the deterministic Tier-1 evaluators are entirely local and offline. It runs disconnected, on a laptop, in a locked-down CI runner, or on an air-gapped host. distroless/static + nonroot keeps the attack surface to just the binary. (Optional LLM-as-judge / embedding evaluators are BYO-key and call out only when you opt in.)

## License

Apache-2.0.
