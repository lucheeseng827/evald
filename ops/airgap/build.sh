#!/usr/bin/env bash
# Assemble an evald air-gap bundle: everything an install with no network needs, listed in one
# SHA256SUMS for the caller to sign.
#
#   ops/airgap/build.sh --version 0.5.0 --target x86_64-unknown-linux-musl \
#     --binary dist/evald-x86_64-unknown-linux-musl.tar.gz \
#     --image dist/evald-image-amd64.tar --chart ops/helm/evald \
#     --sbom dist/evald.sbom.spdx.json --provenance dist/provenance.jsonl \
#     --release-sums dist/SHA256SUMS --trusted-root trusted_root.json --out dist
#
# It writes <out>/evald-airgap-<version>-<target>/ and its SHA256SUMS. Then, in this order:
#   sign:  cosign sign-blob --yes --bundle <dir>/SHA256SUMS.sigstore.json <dir>/SHA256SUMS
#          (evald's release, keyless), or your own `--key` signature as SHA256SUMS.sig;
#   pack:  tar -C <out> -cf <out>/evald-airgap-<version>-<target>.tar evald-airgap-<version>-<target>
#
# Only --version, --target and --binary are required: what is not given is not in the bundle,
# and the bundle's README says what it holds.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
module="$(cd "$here/../.." && pwd)"
version='' target='' binary='' image='' chart='' sbom='' provenance='' release_sums='' trusted_root=''
out='dist'

die() { echo "airgap/build: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="${2#v}"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --binary) binary="$2"; shift 2 ;;
    --image) image="$2"; shift 2 ;;
    --chart) chart="$2"; shift 2 ;;
    --sbom) sbom="$2"; shift 2 ;;
    --provenance) provenance="$2"; shift 2 ;;
    --release-sums) release_sums="$2"; shift 2 ;;
    --trusted-root) trusted_root="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done
[ -n "$version" ] && [ -n "$target" ] && [ -n "$binary" ] || die "--version, --target and --binary are required"
for f in "$binary" ${image:+"$image"} ${sbom:+"$sbom"} ${provenance:+"$provenance"} \
  ${release_sums:+"$release_sums"} ${trusted_root:+"$trusted_root"}; do
  [ -f "$f" ] || die "no such file: $f"
done

name="evald-airgap-$version-$target"
dir="$out/$name"
rm -rf "$dir"
mkdir -p "$dir"
held=()

cp "$binary" "$dir/$(basename "$binary")"
held+=("$(basename "$binary"): the evald binary for $target")
if [ -n "$image" ]; then
  cp "$image" "$dir/evald-image.tar"
  held+=("evald-image.tar: the evald container image (docker load -i / podman load -i)")
fi
if [ -n "$chart" ]; then
  command -v helm >/dev/null 2>&1 || die "--chart needs helm to package the chart"
  helm package "$chart" --version "$version" --app-version "$version" -d "$dir" >/dev/null
  held+=("evald-$version.tgz: the single-node Helm chart (helm install evald ./evald-$version.tgz)")
fi
cp "$module/src/price_baseline.json" "$dir/price-table.json"
held+=("price-table.json: the price table compiled into this evald, as a starting point for --price-table")
if [ -n "$sbom" ]; then
  cp "$sbom" "$dir/evald.sbom.spdx.json"
  held+=("evald.sbom.spdx.json: the release's SBOM (SPDX)")
fi
if [ -n "$provenance" ]; then
  cp "$provenance" "$dir/provenance.jsonl"
  held+=("provenance.jsonl: the release's SLSA build provenance (Sigstore bundles, one per line)")
fi
if [ -n "$release_sums" ]; then
  mkdir -p "$dir/release"
  for f in "$release_sums" "$release_sums".sig "$release_sums".pem "$release_sums".sigstore.json; do
    [ -f "$f" ] && cp "$f" "$dir/release/"
  done
  held+=("release/: the release's own SHA256SUMS and its signatures, as published")
fi
if [ -n "$trusted_root" ]; then
  cp "$trusted_root" "$dir/sigstore-trusted-root.json"
  held+=("sigstore-trusted-root.json: a snapshot of the Sigstore trusted root (prefer one you bring yourself)")
fi
cp "$here/verify.sh" "$dir/verify.sh"
chmod 0755 "$dir/verify.sh"
held+=("verify.sh: checks every file against SHA256SUMS and its signature, offline")

{
  sed -e "s/@VERSION@/$version/g" -e "s/@TARGET@/$target/g" "$here/bundle-README.md"
  echo
  echo "## What this bundle holds"
  echo
  for h in "${held[@]}"; do echo "- \`${h%%:*}\`:${h#*:}"; done
} > "$dir/README.md"

( cd "$dir" && find . -type f ! -path './SHA256SUMS' ! -path './SHA256SUMS.*' | sed 's|^\./||' | LC_ALL=C sort \
    | while read -r f; do sha256sum "$f"; done > SHA256SUMS )
echo "$dir"
