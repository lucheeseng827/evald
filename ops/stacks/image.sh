#!/usr/bin/env sh
# Build the evald image from an already-built static binary, the way the release does
# (Dockerfile.release: distroless, nonroot, /data owned by the image's user).
#
#   ops/stacks/image.sh target/x86_64-unknown-linux-musl/release/evald evald:judge
#
# CI builds the binary with `--features judge` so the stacks' `eval run`, `judge probe` and
# online evals have a judge backend; the published image is the default build.
set -eu
BIN="${1:?usage: ops/stacks/image.sh <static evald binary> <image tag>}"
TAG="${2:?usage: ops/stacks/image.sh <static evald binary> <image tag>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CTX="$(mktemp -d)"
trap 'rm -rf "$CTX"' EXIT
cp "$BIN" "$CTX/evald-amd64"
chmod +x "$CTX/evald-amd64"
mkdir "$CTX/data"
docker build --build-arg TARGETARCH=amd64 -f "$ROOT/Dockerfile.release" -t "$TAG" "$CTX"
docker run --rm "$TAG" version
