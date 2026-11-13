#!/usr/bin/env bash
# The last connected step of the air-gap check (offline-check.sh runs next, with no network):
# fetch what the air-gapped side will need, every download pinned by checksum, and build a
# bundle from an already-built static binary, signed with a throwaway importer's key.
#
#   ops/airgap/ci-prepare.sh <static evald binary> <work dir> [--image]
#
# --image also builds the release image from the binary and puts it in the bundle (needs
# docker), so the offline side loads and runs it.
set -euo pipefail
bin="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
work="$2"
image="${3:-}"
here="$(cd "$(dirname "$0")" && pwd)"
module="$(cd "$here/../.." && pwd)"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
tools="$work/tools"
mkdir -p "$tools"

fetch() { # url sha256 out
  curl -fsSL --retry 3 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c - >/dev/null || { echo "ci-prepare: checksum mismatch for $1" >&2; exit 1; }
}

# cosign: the version verify.sh documents as the floor.
fetch https://github.com/sigstore/cosign/releases/download/v2.4.1/cosign-linux-amd64 \
  8b24b946dd5809c6bd93de08033bcf6bc0ed7d336b7785787c080f574b89249b "$tools/cosign"
chmod 0755 "$tools/cosign"
export PATH="$tools:$PATH"

# A Sigstore trusted root, through cosign's own TUF client, as a user would bring one in.
# (cosign 2 caches it under ~/.sigstore/root/targets/, cosign 3 under a directory per mirror.)
HOME="$work/connected-home" cosign initialize >/dev/null
root="$(find "$work/connected-home/.sigstore/root" -name trusted_root.json | head -1)"
[ -n "$root" ] || { echo "ci-prepare: cosign initialize left no trusted_root.json" >&2; exit 1; }
cp "$root" "$work/trusted_root.json"

# A keyless-signed file and its Sigstore bundle (cosign's own v3.0.2 release), so the offline
# side shows the keyless path verifies with no network.
base=https://github.com/sigstore/cosign/releases/download/v3.0.2
fetch "$base/cosign_checksums.txt" \
  261875ae0067f84dc0b4bfe44275392f06a258362acc14516861e5d8fca17dbd "$work/cosign_checksums.txt"
fetch "$base/cosign_checksums.txt.sigstore.json" \
  14c559dcca7619074951d49f81a31062543a67b3e7293e8a468fcec2e7e439e6 "$work/cosign_checksums.txt.sigstore.json"

# The bundle: this binary, its image if asked, the chart.
dist="$work/dist"
rm -rf "$dist" && mkdir -p "$dist"
cp "$bin" "$dist/evald"
tar -C "$dist" -czf "$dist/evald-x86_64-unknown-linux-musl.tar.gz" evald
args=(--version 0.0.0-ci --target x86_64-unknown-linux-musl
  --binary "$dist/evald-x86_64-unknown-linux-musl.tar.gz"
  --chart "$module/ops/helm/evald" --trusted-root "$work/trusted_root.json" --out "$dist")
if [ "$image" = --image ]; then
  "$module/ops/stacks/image.sh" "$bin" evald:0.0.0-ci
  docker save -o "$dist/evald-image.tar" evald:0.0.0-ci
  docker rmi evald:0.0.0-ci >/dev/null
  args+=(--image "$dist/evald-image.tar")
fi
dir="$("$here/build.sh" "${args[@]}")"

# Signed as an organisation re-signs an import: its own key, no transparency log.
export COSIGN_PASSWORD=""
rm -f "$work/import.key" "$work/import.pub"
(cd "$work" && cosign generate-key-pair --output-key-prefix import >/dev/null)
cosign sign-blob --yes --key "$work/import.key" --tlog-upload=false \
  --output-signature "$dir/SHA256SUMS.sig" "$dir/SHA256SUMS" >/dev/null
rm -f "$work/import.key"
tar -C "$dist" -cf "$work/bundle.tar" "$(basename "$dir")"
echo "ci-prepare: $work/bundle.tar ($(du -h "$work/bundle.tar" | cut -f1)), ready for offline-check.sh"
