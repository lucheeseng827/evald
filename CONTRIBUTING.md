# Contributing to evald

Thanks for your interest. evald (the binary, the store, the eval runner, the
embedded UI) is **Apache-2.0** and is the same code that runs in production.

## Developer Certificate of Origin (DCO)

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/).
Sign off every commit:

```bash
git commit -s -m "your message"
```

The `Signed-off-by` line certifies you wrote the patch or have the right to submit
it under the project's license.

## Ground rules that keep the project honest

- **Default build stays pure-Rust, no network, no container, no C/C++.** evald
  targets air-gapped / regulated / offline-CI use — "data never leaves the box,
  runs in a locked-down CI runner" is a feature, not an accident. Anything
  heavy or network-bound (Tier-2 semantic / HNSW, Tier-3 LLM-as-judge) goes behind
  a cargo feature, **off by default**, and is **BYO-key** when it does make a call.
  evald never phones home.
- **The WAL append is the ACK boundary.** Ingest correctness is non-negotiable. Never ACK
  a span before it is fsynced; never silently drop under load — shed explicitly with
  `429/503 + Retry-After`. Any change to the hot→cold commit protocol (PLAN.md §1.3)
  must keep the `kill -9`-during-compaction recovery test green.
- **One span model, lossless ingest.** Normalize OpenInference and `gen_ai.*` into
  the single `NormalizedSpan`; never hard-fail on an unrecognized attribute — retain
  the raw form in `raw_attributes`. Add a fixture for every convention/version you
  parse, and pin the semconv version the mapping targets.
- **Tier-1 evaluators are deterministic and zero-cost** (no network, no LLM, no user
  shell/wasm). Keep the built-in set fixed and side-effect-free; add unit tests for
  every evaluator.
- **Be honest in docs.** Don't add overclaims ("never drops", "drop-in compatible"
  before it's verified) or unmeasured performance claims.
- **Keep the core building alone.** The Apache core must compile and test on its own.

## Building & testing

```bash
cargo test                  # store + ingest + eval unit/integration tests
cargo run   -- serve        # OTLP/HTTP :4318 + query API + embedded UI
cargo run   -- eval run --config examples/eval/eval.yaml
cargo run   -- query "SELECT model, COUNT(*) FROM spans GROUP BY model"
```

See `PLAN.md` for the phased plan (PoC → MVP → Beta → GA), the architecture, the
data model, and the storage risks the design owns.
