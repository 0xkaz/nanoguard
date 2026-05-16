> **Status:** shipped (2026-05-16)

# Trivy filesystem + image scanning in CI

This note records why the project adopted Aqua Security's Trivy
as a built-in scanner in `ci.yml` and as a local `make` target,
what it scans, and what it intentionally does not replace.

## Scope

- **What this is**: a required CI job that installs Trivy at a
  pinned version, scans the working tree for vulnerabilities,
  misconfigurations, and secrets, and uploads the SARIF result to
  GitHub code scanning. The same scanner is available locally via
  `make trivy` (filesystem) and `make trivy-image` (container
  image). See `.github/workflows/ci.yml > trivy` and the
  `install-trivy` recipe in the Makefile.
- **What this is not**: a replacement for `cargo audit` (which
  already runs as a required check against the RustSec advisory
  DB), not a replacement for `make geiger` (which surfaces unsafe
  Rust usage in dependencies), and not a substitute for the
  opt-in Aikido SCA workflow (see [aikido-sca.md](./aikido-sca.md)).

## Why add Trivy

`cargo audit` and `make geiger` cover the Rust dependency graph
specifically: known advisories for crates we depend on, and unsafe
code usage within those crates. They do not scan:

- The container image we publish to `ghcr.io/0xkaz/nanoguard`.
  Base-image CVEs, OS-package CVEs, and shipped binaries other
  than `nanoguard` itself are invisible to `cargo audit`.
- Repository misconfigurations (Dockerfile lint, exposed secrets,
  IaC drift) that have nothing to do with the Cargo lockfile.
- Filesystem-level secret leaks (committed `.env`, embedded
  credentials in YAML, etc.).

Trivy fills those gaps with one tool. Running it both in CI and
locally means a developer can reproduce the CI finding before
pushing, which is important because SARIF uploads only surface in
the GitHub Security tab after the PR has already run.

## Adopt / reject decision

**Adopted.** Trivy is free, runs in <30s on this repo, pins
cleanly to a specific version via `apt`, and emits SARIF that
GitHub Advanced Security renders inline on the PR diff. The
alternatives considered:

- **Grype + Syft** (Anchore): comparable scan quality, but two
  binaries and two install paths. Trivy bundles the same surface
  into one binary.
- **Snyk OSS / Snyk Container**: stronger UI, but the free tier
  is per-developer-seat and the paid tier exceeds what we want to
  spend at this stage. See the Snyk note in
  [aikido-sca.md](./aikido-sca.md) for the cost comparison.
- **Aikido alone**: Aikido covers SCA but, on the free tier, does
  not scan container images or filesystem secrets. Trivy and
  Aikido are complementary rather than overlapping.

## Pinning and reproducibility

The Trivy version is pinned to `TRIVY_VERSION = 0.50.1` in both
the Makefile and the CI workflow. Local `make trivy` and the CI
`trivy scan` job therefore install and run the same binary, so a
finding that appears in CI can be reproduced locally with the
same SARIF output. Override locally with
`make trivy TRIVY_VERSION=X.Y.Z` when investigating a regression
in a newer release; do not bump the version in `main` without
re-confirming the SARIF schema is stable against
`github/codeql-action/upload-sarif@v3`.

## DB caching

Trivy ships a vulnerability database that updates roughly every
six hours upstream. The CI job caches `~/.cache/trivy` keyed on
`github.run_id`, with `${{ runner.os }}-trivy-db-` as the
`restore-keys` fallback. The result: each run saves a fresh
cache, and a cache miss still restores the most recent prior DB
rather than re-downloading from scratch.
