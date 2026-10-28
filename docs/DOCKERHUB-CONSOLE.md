# evald-console

Standalone frontend for **evald** — the OTel-native trace + eval store for LLM apps.

The console normally ships **baked into the `evald` binary** (served at `/` on `:4318`),
so you usually don't need this image. It exists for deployments that serve the UI
**separately** from the store: behind a CDN/edge, scaled apart from the backend, or
fronting a headless evald node.

It's an nginx image serving the compiled console with an SPA fallback and a reverse proxy
for the `/v1/*` API to a real evald node.

## Run

```bash
# point it at your evald store (the OTLP/HTTP API node)
docker run -p 8080:8080 \
  -e EVALD_API_URL=http://your-evald:4318 \
  mancube/evald-console
# open http://localhost:8080
```

`EVALD_API_URL` (default `http://evald:4318`) is substituted into the proxy at container
start — the same image points at any backend without a rebuild.

## docker compose

The repository's [`compose.yaml`](https://github.com/lucheeseng827/evald/blob/main/compose.yaml)
runs both, with the store on a named volume and the ports on `127.0.0.1`:

```yaml
services:
  evald:
    image: mancube/evald            # serves on 4318 + 4317 into /data by default (0.4.0+)
    ports: ["127.0.0.1:4318:4318", "127.0.0.1:4317:4317"]
    volumes: ["evald-data:/data"]
  console:
    image: mancube/evald-console
    environment:
      EVALD_API_URL: http://evald:4318
    ports: ["127.0.0.1:8080:8080"]   # the UI
    depends_on: [evald]
volumes: { evald-data: {} }
```

Point your app's OpenTelemetry / OpenInference exporter at the **evald** service
(`:4318`) — see the instrumentation guide in the repo — and open the console on `:8080`.

## Tags

- `mancube/evald-console:latest` — the latest promoted release.
- `mancube/evald-console:vX.Y.Z` — pinned to a release.

Multi-arch: `linux/amd64` + `linux/arm64`. Listens on `:8080`, runs unprivileged.

- Store image: **`mancube/evald`** · Source + docs:
  <https://github.com/lucheeseng827/evald> · Apache-2.0.
