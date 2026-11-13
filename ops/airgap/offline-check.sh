#!/usr/bin/env bash
# The air-gap acceptance check (ROADMAP-0.4-0.6.md S6.4): a machine with no network installs,
# verifies signatures and upgrades from the bundle. Run it with the network gone, e.g.
#
#   sudo env "PATH=$PATH" unshare -n ops/airgap/offline-check.sh <work> tests/fixtures/format-v1
#
# <work> holds what the last connected step fetched:
#   bundle.tar              the air-gap bundle, its SHA256SUMS signed with import.key
#   import.pub              the importer's public key (the --key path of verify.sh)
#   trusted_root.json       a Sigstore trusted root (`cosign initialize` on a connected machine)
#   cosign_checksums.txt    a keyless-signed file and its Sigstore bundle (cosign's own release),
#   cosign_checksums.txt.sigstore.json   to show the keyless path verifies offline too
# The fixture is a data-dir written by evald 0.2.0 (the format freeze's recording).
set -euo pipefail
work="$(cd "$1" && pwd)"
fixture="$(cd "$2" && pwd)"
step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
# No TUF cache, no credentials: only what was brought in.
export HOME="$work/home"
rm -rf "$HOME" && mkdir -p "$HOME"

step "there is no network (so the rest is proved offline)"
if curl -sS --max-time 5 -o /dev/null https://github.com 2>/dev/null; then
  fail "github.com is reachable: this check must run with the network gone"
fi
echo "ok: no route out"

step "verify the bundle: checksums and the importer's signature"
rm -rf "$work/host" && mkdir -p "$work/host"
tar -C "$work/host" -xf "$work/bundle.tar"
bundle="$(echo "$work"/host/evald-airgap-*)"
[ -d "$bundle" ] || fail "no bundle directory in bundle.tar"
sh "$bundle/verify.sh" --key "$work/import.pub"

step "an altered bundle is refused"
rm -rf "$work/tampered" && cp -r "$bundle" "$work/tampered"
echo '#' >> "$work/tampered/price-table.json"
if sh "$work/tampered/verify.sh" --key "$work/import.pub" 2>/dev/null; then fail "an altered file passed"; fi
echo "ok: refused"

step "a keyless Sigstore bundle verifies offline (evald's release signature path)"
cosign verify-blob --offline --new-bundle-format --trusted-root "$work/trusted_root.json" \
  --bundle "$work/cosign_checksums.txt.sigstore.json" \
  --certificate-identity keyless@projectsigstore.iam.gserviceaccount.com \
  --certificate-oidc-issuer https://accounts.google.com "$work/cosign_checksums.txt"

step "install the binary from the bundle"
rm -rf "$work/prefix" && mkdir -p "$work/prefix"
tar -C "$work/prefix" -xzf "$bundle"/evald-*.tar.gz
evald="$work/prefix/evald"
"$evald" version

if [ -f "$bundle/evald-image.tar" ] && command -v docker >/dev/null 2>&1; then
  step "load the image from the bundle and run it with no network"
  tag="$(docker load -i "$bundle/evald-image.tar" | sed -n 's/^Loaded image: //p' | head -1)"
  [ -n "$tag" ] || fail "docker load named no image"
  docker run --rm --network none "$tag" version
fi

if ls "$bundle"/evald-*.tgz >/dev/null 2>&1 && command -v helm >/dev/null 2>&1; then
  step "the chart in the bundle renders with no network"
  helm template evald "$bundle"/evald-*.tgz --set image.repository=registry.internal/evald >/dev/null
  echo "ok"
fi

step "upgrade: the preflight, then open a store written by evald 0.2.0"
rm -rf "$work/data" && cp -r "$fixture" "$work/data"
"$evald" migrate --check --data-dir "$work/data" | tee "$work/check.out"
grep -q "written by evald 0.2.0" "$work/check.out" || fail "the preflight did not read the old store's marker"
"$evald" query "SELECT count(*) AS n FROM spans" --data-dir "$work/data" 2>/dev/null | tee "$work/query.out"
grep -Eq '"n": ?5' "$work/query.out" || fail "the upgraded store does not hold its 5 spans"

step "a store from a newer evald is refused, not opened"
printf '{"format": 99, "evald_version": "99.0.0", "created_unix_ms": 1}' > "$work/data/FORMAT"
if "$evald" migrate --check --data-dir "$work/data" >/dev/null; then fail "a newer format passed the preflight"; fi
echo "ok: refused"

printf '\nPASS: verified, installed and upgraded with no network\n'
