# evald console — standalone frontend image.
#
# The console normally ships baked INTO the evald binary (rust-embed, served at `/`), so a
# separate image is optional. It exists for deployments that serve the UI independently of
# the store: behind a CDN/edge, scaled apart from the backend, or fronting a headless evald.
#
# Two stages: build the Vite bundle with Node, then serve the static `dist/` from nginx with
# an SPA fallback + a reverse proxy for the `/v1/*` API to a real evald node. The upstream is
# configured at container start via EVALD_API_URL (default http://evald:4318), so the same
# image points at any backend without a rebuild.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t mancube/evald-console ./frontend
#   docker run -p 8080:8080 -e EVALD_API_URL=http://my-evald:4318 mancube/evald-console

# ---- build ----------------------------------------------------------------
FROM node:22-alpine AS build
WORKDIR /app
# Install deps against the lockfile first for layer caching.
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

# ---- serve ----------------------------------------------------------------
FROM nginxinc/nginx-unprivileged:1.27-alpine
LABEL org.opencontainers.image.source="https://github.com/lucheeseng827/evald" \
      org.opencontainers.image.description="evald console — standalone frontend (nginx) for the OTel-native trace + eval store" \
      org.opencontainers.image.licenses="Apache-2.0"

# nginx.conf is a template: EVALD_API_URL is substituted into the /v1 proxy_pass at start
# by the base image's docker-entrypoint (it runs envsubst over *.template in this dir).
ENV EVALD_API_URL=http://evald:4318
COPY --from=build /app/dist /usr/share/nginx/html
COPY nginx.conf.template /etc/nginx/templates/default.conf.template

# The unprivileged base already runs as uid 101 and listens on 8080.
EXPOSE 8080
