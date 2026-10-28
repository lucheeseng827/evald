# evald — distroless image for `evald`.
#
# The default build is Rust apart from one bundled C library: zstd (`zstd-sys`, reached through
# DataFusion → arrow-ipc), compiled from source by its build script, so it needs a musl C
# compiler (Alpine's `musl-dev` toolchain here; `musl-gcc` for a host build of the
# x86_64-unknown-linux-musl target; the release uses `cross`). The binary links fully static
# against musl and drops into `distroless/static`: no libc, no shell, no package manager, runs
# as nonroot. Multi-arch (linux/amd64 + linux/arm64) is produced by `docker buildx`: the build
# stage runs per target platform, so `cargo build` emits the native static binary for each
# without a cross-linker.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t mancube/evald .
#
# NOTE: the DataFusion/Arrow tree is large, so a from-source build (this Dockerfile) is slow,
# especially the emulated arm64 leg. CI uses Dockerfile.release (prebuilt binaries) instead;
# this file is for a from-source local build. The embedded SPA is baked in via rust-embed (no
# Node toolchain needed).

# An exact version rather than `rust:1-alpine`. A floating base means the compiler that
# built a published artefact changed because an image moved, not because anyone decided
# it should — which is hard to square with shipping an SBOM and build provenance
# alongside it. Bump deliberately, and expect the bump to be the commit that fixes
# whatever the new release warns about.
FROM rust:1.98.1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Alpine's host target is *-unknown-linux-musl → a fully static binary by default.
RUN cargo build --release --bin evald && \
    strip target/release/evald && \
    mkdir -p /data-empty

FROM gcr.io/distroless/static-debian12:nonroot
LABEL org.opencontainers.image.source="https://github.com/lucheeseng827/evald" \
      org.opencontainers.image.description="Embedded OTel-native trace + eval store for LLM apps (single binary)" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /src/target/release/evald /usr/local/bin/evald
# The same defaults as Dockerfile.release (see there): serve on both ports into the /data
# volume, which belongs to the image's user so a fresh named volume is writable.
COPY --from=build --chown=65532:65532 /data-empty /data
ENV EVALD_OTLP_HTTP_ADDR=0.0.0.0:4318 \
    EVALD_OTLP_GRPC_ADDR=0.0.0.0:4317 \
    EVALD_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 4317 4318
ENTRYPOINT ["/usr/local/bin/evald"]
CMD ["serve"]
