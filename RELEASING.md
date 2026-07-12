# Releasing evald

Releases are cut by tag. `.github/workflows/release.yml` does the rest.

## Cut a release

1. Bump `version` in `Cargo.toml`, move the `CHANGELOG` `[Unreleased]` entries under the new
   version, land on `main`.
2. Tag and push:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The `release` workflow then, on that tag:
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

- `SHA256SUMS` — checksums, plus `SHA256SUMS.sig` + `SHA256SUMS.pem` (cosign **keyless** signature).
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
