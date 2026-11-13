#!/bin/sh
# Verify an evald air-gap bundle with no network: run it from inside the extracted bundle.
#
#   sh verify.sh --trusted-root /media/usb/trusted_root.json   # evald's keyless release signature
#   sh verify.sh --key /etc/pki/evald-import.pub                # your organisation's own signature
#   sh verify.sh --checksums-only                               # no signature check (says so)
#
# In order, stopping at the first thing that does not check out:
#   1. every file in the bundle is listed in its SHA256SUMS, and matches it (nothing missing,
#      nothing added);
#   2. SHA256SUMS itself is signed, either by evald's release workflow (keyless:
#      SHA256SUMS.sigstore.json, verified offline against a Sigstore trusted root) or by a key
#      you hold (SHA256SUMS.sig, made with `cosign sign-blob --key` when you imported the bundle).
#
# The keyless check needs cosign 2.4 or later and a Sigstore trusted root. Bring that root in
# separately from the bundle: on a connected machine, `cosign initialize`, then copy
# trusted_root.json from ~/.sigstore/root/targets/ (cosign 2) or
# ~/.sigstore/root/tuf-repo-cdn.sigstore.dev/targets/ (cosign 3). The bundle carries a
# snapshot (sigstore-trusted-root.json) for when you cannot, but a root that travels with the
# files it vouches for is only as trustworthy as the channel that brought them: this script
# prints its digest so that you can compare it against one you trust.
set -eu

IDENTITY='^https://github\.com/lucheeseng827/evald/\.github/workflows/release\.yml@refs/tags/v.+'
ISSUER='https://token.actions.githubusercontent.com'
TRUSTED_ROOT=''
KEY=''
CHECKSUMS_ONLY=0

say() { printf 'evald-verify: %s\n' "$*" >&2; }
die() { say "FAIL: $*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --trusted-root) [ $# -ge 2 ] || die "--trusted-root needs a file"; TRUSTED_ROOT="$2"; shift 2 ;;
    --key) [ $# -ge 2 ] || die "--key needs a public key file"; KEY="$2"; shift 2 ;;
    --identity) [ $# -ge 2 ] || die "--identity needs a regexp"; IDENTITY="$2"; shift 2 ;;
    --checksums-only) CHECKSUMS_ONLY=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

# Paths given on the command line are the caller's, not the bundle's.
absolute() { case "$1" in ''|/*) printf '%s' "$1" ;; *) printf '%s/%s' "$PWD" "$1" ;; esac; }
KEY="$(absolute "$KEY")"
TRUSTED_ROOT="$(absolute "$TRUSTED_ROOT")"
cd "$(dirname "$0")"
[ -f SHA256SUMS ] || die "no SHA256SUMS here: run this from inside the extracted bundle"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum"; fi
}

# 1. Every listed file matches, and every file present is listed.
n=0
while read -r sum name; do
  name="${name#\*}"
  [ -n "$name" ] || continue
  [ -f "$name" ] || die "$name is listed in SHA256SUMS but missing"
  [ "$(sha256_of "$name")" = "$sum" ] || die "$name does not match SHA256SUMS: altered or damaged"
  n=$((n + 1))
done < SHA256SUMS
find . -type f ! -path './SHA256SUMS' ! -path './SHA256SUMS.*' | sed 's|^\./||' | while read -r f; do
  awk -v f="$f" '{ g = $2; sub(/^\*/, "", g); if (g == f) found = 1 } END { exit !found }' SHA256SUMS \
    || die "$f is in the bundle but not in SHA256SUMS: something was added"
done
say "checksums: $n file(s) match SHA256SUMS, and nothing else is here"

# 2. SHA256SUMS is signed.
if [ "$CHECKSUMS_ONLY" = 1 ]; then
  say "signature NOT checked (--checksums-only): the files are as SHA256SUMS lists them, which is only as good as SHA256SUMS"
  exit 0
fi
command -v cosign >/dev/null 2>&1 || die "cosign (2.4 or later) is not installed: install it from the bundle's own channel, or pass --checksums-only to skip the signature knowingly"

if [ -n "$KEY" ]; then
  [ -f SHA256SUMS.sig ] || die "--key given but there is no SHA256SUMS.sig (sign it on import: cosign sign-blob --yes --key <key> --tlog-upload=false --output-signature SHA256SUMS.sig SHA256SUMS, adding --use-signing-config=false --new-bundle-format=false with cosign 3)"
  cosign verify-blob --key "$KEY" --signature SHA256SUMS.sig --insecure-ignore-tlog --offline SHA256SUMS >/dev/null 2>&1 \
    || die "SHA256SUMS is not signed by the key in $KEY"
  say "signature: SHA256SUMS is signed by the key in $KEY"
  exit 0
fi

[ -f SHA256SUMS.sigstore.json ] || die "no SHA256SUMS.sigstore.json (evald's keyless signature bundle); pass --key for your own signature, or --checksums-only"
if [ -z "$TRUSTED_ROOT" ]; then
  [ -f sigstore-trusted-root.json ] || die "no Sigstore trusted root: pass --trusted-root <trusted_root.json>"
  TRUSTED_ROOT=sigstore-trusted-root.json
  say "WARNING: using the trusted root that came inside this bundle (sha256 $(sha256_of "$TRUSTED_ROOT")); compare that digest against a trusted_root.json you obtained another way, or pass --trusted-root"
fi
cosign verify-blob --offline --new-bundle-format --trusted-root "$TRUSTED_ROOT" \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp "$IDENTITY" --certificate-oidc-issuer "$ISSUER" \
  SHA256SUMS >/dev/null 2>&1 \
  || die "SHA256SUMS is not signed by evald's release workflow (identity $IDENTITY)"
say "signature: SHA256SUMS is signed by evald's release workflow, verified offline"
