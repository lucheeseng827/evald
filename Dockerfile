# evald — distroless image for `evald`.
#
# The default build is pure-Rust with zero C/C++ deps (DataFusion/Arrow/Parquet, redb, and the
# axum stack are all pure-Rust), so the binary links fully static against musl (Alpine's native
# target) and drops into `distroless/static` — no libc, no shell, no package manager, runs as
# nonroot. Multi-arch (linux/amd64 + linux/arm64) is produced by `docker buildx`: the build
# stage runs per target platform, so `cargo build` emits the native static binary for each
# without a cross-linker.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t mancube/evald .
#
# NOTE: the DataFusion/Arrow tree is large, so a from-source build (this Dockerfile) is slow,
# especially the emulated arm64 leg. CI uses Dockerfile.release (prebuilt binaries) instead;
# this file is for a from-source local build. The embedded SPA is baked in via rust-embed (no
# Node toolchain needed).

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Alpine's host target is *-unknown-linux-musl → a fully static binary by default.
RUN cargo build --release --bin evald && \
    strip target/release/evald

FROM gcr.io/distroless/static-debian12:nonroot
LABEL org.opencontainers.image.source="https://github.com/lucheeseng827/evald" \
      org.opencontainers.image.description="Embedded OTel-native trace + eval store for LLM apps (single binary)" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /src/target/release/evald /usr/local/bin/evald
# `evald serve` OTLP/HTTP + API + UI port.
EXPOSE 4318
ENTRYPOINT ["/usr/local/bin/evald"]
CMD ["version"]
