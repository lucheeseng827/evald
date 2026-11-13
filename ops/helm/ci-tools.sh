#!/usr/bin/env sh
# Install the pinned helm, kind and kubectl the chart checks use into DIR (default ./bin),
# each verified against its published SHA-256 before it is made executable.
#
#   ops/helm/ci-tools.sh "$RUNNER_TEMP/bin" && echo "$RUNNER_TEMP/bin" >> "$GITHUB_PATH"
set -eu
DIR="${1:-bin}"
mkdir -p "$DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fetch() { # url sha256 out
  curl -fsSL --retry 3 -o "$TMP/dl" "$1"
  echo "$2  $TMP/dl" | sha256sum -c - >/dev/null || { echo "ci-tools: checksum mismatch for $1" >&2; exit 1; }
  mv "$TMP/dl" "$3"
}

HELM_VERSION=v3.16.4
HELM_SHA256=fc307327959aa38ed8f9f7e66d45492bb022a66c3e5da6063958254b9767d179
KIND_VERSION=v0.31.0
KIND_SHA256=eb244cbafcc157dff60cf68693c14c9a75c4e6e6fedaf9cd71c58117cb93e3fa
KUBECTL_VERSION=v1.37.1
KUBECTL_SHA256=65691ff77eb6fa44c908b77a1082c9f092c3b9733b5cefabec0d1104890e21a8

fetch "https://get.helm.sh/helm-${HELM_VERSION}-linux-amd64.tar.gz" "$HELM_SHA256" "$TMP/helm.tgz"
tar -xzf "$TMP/helm.tgz" -C "$TMP" linux-amd64/helm
mv "$TMP/linux-amd64/helm" "$DIR/helm"
fetch "https://github.com/kubernetes-sigs/kind/releases/download/${KIND_VERSION}/kind-linux-amd64" "$KIND_SHA256" "$DIR/kind"
fetch "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" "$KUBECTL_SHA256" "$DIR/kubectl"
chmod +x "$DIR/helm" "$DIR/kind" "$DIR/kubectl"
"$DIR/helm" version --short
"$DIR/kind" version
"$DIR/kubectl" version --client
