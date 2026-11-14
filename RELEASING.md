# Releasing evald

Releases are cut by tag. `.github/workflows/release.yml` does the rest.

## Cut a release

1. Refresh the built-in price table (`src/price_baseline.json`, a trimmed copy of LiteLLM's), so
   the release prices the models that shipped since the last one:
   ```bash
   python scripts/gen_price_baseline.py        # the latest upstream commit, via the gh CLI
   # or, without the GitHub API, from a clone (a blob-less shallow one is enough):
   git clone --filter=blob:none --no-checkout --depth 50 https://github.com/BerriAI/litellm /tmp/litellm
   python scripts/gen_price_baseline.py --clone /tmp/litellm
   cargo test
   ```
   It prints how many models were added, dropped upstream and repriced. Say so in the
   `CHANGELOG`: a model upstream dropped is no longer priced, unless an operator's
   `--price-table` names it.
2. Bump `version` in `Cargo.toml`, move the `CHANGELOG` `[Unreleased]` entries under the new
   version, land on `main`.
3. Tag and push:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
4. The `release` workflow then, on that tag:
   - **builds** static-musl (`x86_64`/`aarch64`), macOS (`x86_64`/`arm64`), and Windows
     (`x86_64`) binaries — each archived as `evald-<target>.{tar.gz,zip}` with a `.sha256`;
   - **publishes** the GitHub release with every artifact + an aggregated `SHA256SUMS`;
   - **publishes** the `evald` crate to crates.io (real, non-rc tags only; idempotent);
   - **bumps** `Formula/evald.rb` (version + the four desktop sha256) and commits it to `main`,
     so `brew upgrade evald` serves the new build;
   - **pushes** a multi-arch (`linux/amd64` + `linux/arm64`) distroless image to
     `docker.io/mancube/evald:{vX.Y.Z,latest}`.

A pre-release tag (`vX.Y.Z-rc.N`) is marked a GitHub pre-release and is intentionally **not**
promoted to crates.io / brew / `:latest` — use it to smoke-test the pipeline.
`workflow_dispatch` re-runs the pipeline against an existing tag (input `tag`), with an optional
`promote` to advance the formula + `:latest`.

## Folding or merging a long-lived branch

A conflicting file is never resolved by taking one side (`git checkout --ours/--theirs`, or
copying one branch's copy over). Both sides changed it since they parted, and taking one drops
the other's changes without a trace. 0.5.0's fold of the 0.6 branch did exactly that: it applied
0.6's copies of files over the review fixes `main` had taken in the meantime (#1644), dropping
fixes and three tests, and 0.5.0 and 0.5.1 had to put them back.

1. Find what each side changed since the merge base, per conflicting file:
   ```bash
   base=$(git merge-base main the-branch)
   git diff "$base" main -- path/to/file          # what main brought
   git diff "$base" the-branch -- path/to/file    # what the branch brought
   ```
2. Carry both into the result. Where they touch the same lines, write the line that does what
   both meant, and say so in the commit message.
3. Before landing, look for what `main` brought and the result lacks, from the repository root
   (in a subdirectory, `git apply` skips every path outside it and passes without a word):
   ```bash
   cd "$(git rev-parse --show-toplevel)"
   git diff "$base" main | git apply --check -R
   ```
   On the merged tree this names each file where a hunk of `main`'s is not there. It stops at a
   file's first such hunk, and a hunk both sides rewrote counts, so for every file it names, read
   all of `git diff "$base" main -- that/file` against the result, not only the hunk it reports.
4. Run every test both sides added. A test deleted in the merge needs a reason in the commit
   message.

## Distribution channels

| Channel | Source of truth |
|---|---|
| `cargo binstall evald` | release tarballs; asset names from `[package.metadata.binstall]` in `Cargo.toml` |
| `cargo install evald` | crates.io (published by the release workflow on each real tag) |
| `brew install evald` | `Formula/evald.rb` (this repo is its own tap) |
| `docker run … mancube/evald` | `Dockerfile` (distroless, self-building musl) |
| `cargo install --git …` | the crate source |

## Required mirror secrets

| Secret / env | Used by | What |
|---|---|---|
| `CRATESIO_TOKEN` (env `cicd`) | crates job | crates.io API token with publish scope |
| `DOCKER_USERNAME` / `DOCKER_PASSWORD` (env `cicd`) | docker job | Docker Hub user `mancube` + an access token with Read/Write on `mancube/evald` |

## Verify a build locally

```bash
# the exact static target the release ships
cargo build --release --target x86_64-unknown-linux-musl --bin evald

# the container (from source; slow — the DataFusion tree is large)
docker buildx build --platform linux/amd64,linux/arm64 -t evald:dev .
```

## Verifying a release (supply-chain)

Every tagged release ships signed + attested artifacts so you can prove what you downloaded is
what CI built — the "clean supply chain, no rug-pull" property (contrast the LiteLLM PyPI
compromise). Alongside the binaries:

- `SHA256SUMS` — checksums, plus `SHA256SUMS.sig` + `SHA256SUMS.pem` (cosign **keyless** signature) and, from 0.5, `SHA256SUMS.sigstore.json` (the same signature as a Sigstore bundle, which verifies offline).
- `evald-airgap-<version>-x86_64-unknown-linux-musl.tar` (from 0.5) — the air-gap bundle: binary, image archive, chart, price table, SBOM, provenance, trusted root and `verify.sh` ([OPERATIONS.md § Air-gapped install and upgrade](docs/OPERATIONS.md#air-gapped-install-and-upgrade)).
- `evald.sbom.spdx.json` — an SPDX SBOM of the release artifacts.
- **SLSA build provenance** — a GitHub-native attestation over each archive.

```bash
# 1. Verify the checksum signature (keyless, via the GitHub Actions OIDC identity):
cosign verify-blob \
  --certificate SHA256SUMS.pem --signature SHA256SUMS.sig \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github\.com/lucheeseng827/evald/\.github/workflows/release\.yml@.+' \
  SHA256SUMS
sha256sum -c SHA256SUMS       # then confirm the binary matches

# 2. Verify SLSA build provenance for a binary:
gh attestation verify evald-x86_64-unknown-linux-musl.tar.gz --repo <owner>/evald
```
