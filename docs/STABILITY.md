# What 1.0 freezes

evald 1.0 is gated on evald's own surface being frozen and the semantic conventions it reads
being pinned (decision D7). It is not gated on the OpenTelemetry GenAI conventions leaving
Development, which has no public date. This page is the freeze review: each surface a user,
script or pipeline depends on, what about it is promised, and the test that fails when the
promise is broken.

| Surface | Promise from 1.0 | Held by |
|---|---|---|
| CLI commands, flags, positional arguments, `EVALD_*` variables | None removed or renamed within 1.x. New ones are added only on purpose. | `tests/surface.rs`, against `tests/fixtures/surface/cli.txt` (generated from the binary's `--help`). |
| Exit codes | `0` success, `1` a gate failed, `2` evald could not run. `SIGTERM` drains and exits `0`. | `tests/cli_contract.rs` |
| HTTP routes | Every route in `tests/fixtures/surface/http.txt` answers. | `tests/surface.rs` |
| OTLP/HTTP ingest | The OTLP/HTTP spec. Protobuf and JSON in, gzip or not. The reply is in the request's own Content-Type. Every 4xx/5xx is a `google.rpc.Status` in that encoding. A refusal to store is retryable, with `Retry-After`. | `tests/otlp_conformance.rs` ([API.md § OTLP conformance](./API.md#otlp-conformance)) |
| Any attribute is queryable | Every attribute a span carried can be read in SQL by the key it was sent with: `attr(raw_attributes_json, 'key')`, from the hot tier and from cold Parquet alike. | `tests/any_attribute.rs` ([API.md](./API.md#any-attribute-is-queryable)) |
| SQL tables | The `spans` and `scores` columns in `tests/fixtures/surface/sql.txt` stay, with their names. | `tests/surface.rs` |
| Semantic conventions | The GenAI mapping is written against semconv `1.38.0` (`SEMCONV_VERSION`, also in `GET /v1/meta`). Earlier keys are still read. A newer release's span still normalizes, keeping what the mapping does not know. Raising the pin is a reviewed change. | The `normalize` tests ([INSTRUMENTATION.md § Semantic conventions](./INSTRUMENTATION.md#semantic-conventions)) |
| CI reports | `--report-json` writes `evald.report/v1`. Within v1, fields are only added; a removed, renamed or retyped field is v2. JUnit XML: one case per evaluator. | `docs/schemas/report-v1.schema.json`, validated in the `eval` tests |
| On-disk format | Format 1. A release reads its own format and the one before it. A data-dir is never downgraded. | `tests/format_freeze.rs`, and `evald migrate --check` ([FORMAT.md](./FORMAT.md)) |
| Eval YAML | The keys in [CONFIG.md § Eval YAML](./CONFIG.md#eval-yaml-evald-eval-run---config). An unknown key is an error, so nothing is ever ignored silently. | `deny_unknown_fields`, and the config tests |

## What is not frozen

- **Printed output** (tables, summary lines, log lines) is for people, and changes. A pipeline
  reads `--report-json`, `--junit`, `evald query`'s JSON, or the HTTP API.
- **Metric names on `/metrics`** are documented in [API.md](./API.md#get-metrics) and kept
  stable in practice. They are not part of this freeze until a 1.x release says so.
- **Internals**: the Rust library API (`evald::*`), the WAL and block layout beyond the format
  promise, and the console's own routes.

## Changing a frozen surface

An addition is a reviewed change. Run `EVALD_REGEN_SURFACE=1 cargo test --test
surface`, then read the diff of `tests/fixtures/surface/` in the pull request. A removal or a
rename waits for a major version, and the release notes name it. Until then the old spelling
keeps working beside the new one, as the semconv mapping keeps reading the keys the conventions
deprecated.
