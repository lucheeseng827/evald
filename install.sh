#!/bin/sh
# Install evald from a GitHub release: download, verify, install. No root needed.
#
#   curl -fsSL https://raw.githubusercontent.com/lucheeseng827/evald/main/install.sh | sh
#   curl -fsSL …/install.sh | sh -s -- --version v0.4.0 --dir /usr/local/bin
#
# What it does, in order, and stops at the first thing that does not check out:
#   1. picks the archive for this OS and CPU (Linux → static musl; macOS; Windows users take
#      the .zip from the release page);
#   2. downloads it and the release's SHA256SUMS, and refuses to install unless the archive's
#      checksum is listed there and matches;
#   3. if `cosign` is installed, verifies that SHA256SUMS was signed by evald's release workflow
#      (keyless; see docs/OPERATIONS.md § Verify what you pulled), and refuses if it was not;
#   4. installs `evald` into --dir (default ~/.local/bin) and prints its version.
#
# Mirrors and air-gapped hosts: EVALD_DOWNLOAD_BASE points it at a directory or URL that
# holds the same files as a release (evald-<target>.tar.gz, SHA256SUMS, and optionally
# SHA256SUMS.sig + SHA256SUMS.pem), e.g. an internal artifact store or a file:// path.
set -eu

REPO="${EVALD_REPO:-lucheeseng827/evald}"
VERSION="${EVALD_VERSION:-latest}"
DIR="${EVALD_INSTALL_DIR:-$HOME/.local/bin}"
BASE="${EVALD_DOWNLOAD_BASE:-}"
IDENTITY='^https://github\.com/lucheeseng827/evald/\.github/workflows/release\.yml@.+'

say() { printf 'evald-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a value"; DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0" 2>/dev/null || true; exit 0 ;;
    *) die "unknown argument: $1 (expected --version <tag> or --dir <path>)" ;;
  esac
done

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "no installer for $(uname -s); download the archive from https://github.com/$REPO/releases" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "no release build for CPU $(uname -m)" ;;
esac
target="$arch-$os"
asset="evald-$target.tar.gz"

# Release tags are v-prefixed; take `0.4.0` as well as `v0.4.0`.
case "$VERSION" in
  latest | v*) ;;
  *) VERSION="v$VERSION" ;;
esac

if [ -z "$BASE" ]; then
  if [ "$VERSION" = latest ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
  else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
  fi
fi

fetch() { # url dest
  case "$1" in
    file://*) cp "${1#file://}" "$2" ;;
    /*) cp "$1" "$2" ;;
    *)
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1"
      elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
      else
        die "need curl or wget to download"
      fi ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $asset ($VERSION) from $BASE"
fetch "$BASE/$asset" "$tmp/$asset" || die "could not download $BASE/$asset"
fetch "$BASE/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $BASE/SHA256SUMS; refusing to install an unverified archive"

expected="$(awk -v a="$asset" '{ f = $2; sub(/^\*/, "", f); if (f == a) { print $1; exit } }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS does not list $asset; refusing to install"
actual="$(sha256_of "$tmp/$asset")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual); refusing to install"
say "checksum ok"

if command -v cosign >/dev/null 2>&1; then
  if fetch "$BASE/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig" 2>/dev/null && fetch "$BASE/SHA256SUMS.pem" "$tmp/SHA256SUMS.pem" 2>/dev/null; then
    cosign verify-blob \
      --certificate "$tmp/SHA256SUMS.pem" --signature "$tmp/SHA256SUMS.sig" \
      --certificate-identity-regexp "$IDENTITY" \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com \
      "$tmp/SHA256SUMS" >/dev/null 2>&1 || die "SHA256SUMS is not signed by evald's release workflow; refusing to install"
    say "signature ok (signed by evald's release workflow)"
  else
    die "cosign is installed but the release has no SHA256SUMS.sig/.pem to check; refusing to install"
  fi
else
  say "cosign not installed: checksum verified, signature not checked (docs/OPERATIONS.md § Verify what you pulled)"
fi

tar -xzf "$tmp/$asset" -C "$tmp" evald || die "the archive does not contain an evald binary"
mkdir -p "$DIR"
# A new file renamed into place, so a running evald keeps its old binary until it restarts.
cp "$tmp/evald" "$DIR/.evald.new"
chmod 0755 "$DIR/.evald.new"
mv -f "$DIR/.evald.new" "$DIR/evald"
say "installed $("$DIR/evald" version) to $DIR/evald"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "$DIR is not on your PATH; add it: export PATH=\"$DIR:\$PATH\"" ;;
esac
say "next: evald serve   (then: eval \"\$(evald connect claude-code)\", or evald connect otel-app)"
