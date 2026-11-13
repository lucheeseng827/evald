#!/usr/bin/env bash
# The monorepo CI's test step for this module (.github/workflows/rust.yml runs it when present):
# the default build's suite, then the judge feature's own tests, which only that build compiles
# (the HTTP client and the tests that drive it against a loopback server).
set -euo pipefail
cargo test --verbose -p evald
cargo test --verbose --features judge --lib --test judge_endpoints --test egress --test replay
