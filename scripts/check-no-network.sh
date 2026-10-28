#!/usr/bin/env bash
# The default evald build makes no outbound network call. This script checks that from the
# resolved dependency graph and the source, so it doesn't rest on a promise in a README.
#
#   scripts/check-no-network.sh             check the default build (CI runs this)
#   scripts/check-no-network.sh --selftest  prove the check can fail: the opt-in `judge`
#                                           build MUST be flagged (it carries reqwest + rustls)
#
# Two checks, because neither alone is enough:
#
# 1. The resolved graph of the default feature set (normal edges, every target) contains no
#    HTTP-client library and no TLS stack. Without TLS the binary cannot reach any HTTPS
#    service; without a client library it has no ready-made way to reach a plain-HTTP one.
#    `hyper` and `hyper-util` are deliberately NOT on the list: axum and tonic serve with
#    them, and tonic's `channel` feature (forced on by opentelemetry-proto's `gen-tonic`)
#    compiles hyper's plaintext client in. That code is present but never constructed,
#    which is what check 2 enforces.
# 2. Nothing in src/ constructs an outbound connection through what the default graph does
#    carry: std/tokio sockets, a tonic Channel/Endpoint, or a hyper client.
#
# The `judge` feature is the only sanctioned outbound path (BYO-key LLM judge); it is off by
# default and adds reqwest + rustls, which is exactly what --selftest asserts check 1 sees.
set -euo pipefail

cd "$(dirname "$0")/.."

# Crates whose presence means the binary can make outbound HTTP(S) calls or speak TLS.
banned='^(reqwest|ureq|isahc|curl|curl-sys|surf|attohttpc|awc|hyper-rustls|hyper-tls|hyper-openssl|rustls|tokio-rustls|rustls-native-certs|rustls-platform-verifier|webpki-roots|native-tls|tokio-native-tls|openssl|openssl-sys|boring|boring-sys)$'

graph_hits() { # $@: extra cargo tree args. Fails loudly if the graph can't be read: an
  # empty tree would otherwise pass as "nothing banned".
  local tree
  if ! tree="$(cargo tree -e normal --target all --prefix none -f '{p}' "$@")"; then
    echo "::error::cargo tree failed; the network check cannot run" >&2
    exit 1
  fi
  if ! grep -qE '^axum ' <<<"$tree"; then
    echo "::error::cargo tree output has no axum, so it is not evald's graph" >&2
    exit 1
  fi
  awk '{print $1}' <<<"$tree" | sort -u | grep -E "$banned" || true
}

# Outbound constructors that the default graph makes available. Matched in src/ only: tests
# connect to the local server they started, which is not egress.
outbound='TcpStream::connect|UdpSocket::|transport::Channel|Endpoint::(from|new|try_from)|hyper_util::client|hyper::client|lookup_host'

if [[ "${1:-}" == "--selftest" ]]; then
  hits="$(graph_hits --features judge)"
  if [[ -z "$hits" ]]; then
    echo "::error::selftest: the judge build carries reqwest + rustls, but the graph check saw" \
         "nothing. The check cannot fail, so it proves nothing."
    exit 1
  fi
  echo "OK — selftest: the graph check flags the judge build:" $hits
  exit 0
fi

fail=0
hits="$(graph_hits)"
if [[ -n "$hits" ]]; then
  echo "::error::the default build now carries a network client or TLS stack:" $hits
  echo "Find the edge with: cargo tree -e normal --target all -i <crate>"
  echo "Outbound calls belong behind an opt-in feature (see \`judge\` in Cargo.toml)."
  fail=1
fi

if src_hits="$(grep -rnE "$outbound" src/)"; then
  echo "::error::src/ constructs an outbound connection:"
  echo "$src_hits"
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then exit 1; fi
echo "OK — the default build has no HTTP client library, no TLS stack, and src/ opens no outbound connection."
