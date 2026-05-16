> **Status:** shipped (commit cbfa2c8, 2026-05-17)

# Aikido SCA in CI

The workflow has been exercised end-to-end: the operator-side
enablement (account, `AIKIDO_SECRET_KEY`, `vars.AIKIDO_ENABLED`)
happened in PR #15, and the scan has produced findings against
this repo's dependency graph. The first wave of findings drove
the supply-chain hygiene fixes in PR #20 (dtolnay action pinned
by SHA, distroless runtime image switched to `:nonroot`).

This note records why the project adopted Aikido's dependency-scan
GitHub Action as an opt-in CI step, what it does, and what it
intentionally does not displace.

## Scope

- **What this is**: a single CI workflow that runs Aikido's
  dependency vulnerability scan on push-to-main and on PRs from
  within this repo. Gated by `vars.AIKIDO_ENABLED == 'true'` so it
  is inert until provisioned. See `.github/workflows/aikido.yml`
  and `docs/operations.md > External CI scans`.
- **What this is not**: a replacement for `cargo audit` (which
  already runs as a required check in `ci.yml`), and not a
  replacement for any of the code-review bots (CodeRabbit,
  Greptile, Qodo).

## Why add a fourth bot

The project already had three review bots wired into CI. They are
strong on diff-level reasoning — naming, style, missing tests,
contradiction between docs and code, subtle correctness on the
diff — but none of them scan the **dependency tree** against CVE
feeds. `cargo audit` does, but only at the time CI runs, only
against RustSec's advisory DB, and only for the locked Cargo
graph.

Aikido adds:

- Coverage across the full dependency graph including transitive
  GitHub Actions, container base images (once we ship containers),
  and language ecosystems beyond Rust as the project grows.
- A dashboard and notification surface for findings that survives
  CI log retention.
- A free tier for SCA on public repos that is sufficient for our
  scale.

For the cost of one extra CI step that an operator can ignore
unless they opt in, the project gains a complementary signal.

## Alternatives considered

- **Snyk**: comparable SCA coverage. Free tier exists for OSS.
  Snyk's GitHub Action requires `SNYK_TOKEN` and a snyk.io
  account — same operator-friction shape as Aikido. We did not
  set up both because two SCA scanners reporting the same CVEs is
  duplicate noise; pick one. The decision between the two is
  largely about which dashboard the operator prefers. Easy to
  switch later if Aikido turns out to be the wrong choice.
- **Dependabot alerts**: GitHub-native, zero setup, but reports
  only against the dependency graph GitHub itself parses (Cargo
  is supported but the depth of analysis varies). Useful as a
  complement to a real SCA, not as a replacement.
- **OpenSSF Scorecard**: scores the repo's own security posture,
  not the dependency tree. Different axis.
- **`cargo audit`**: already in CI as a required check. It runs
  RustSec's advisory DB against `Cargo.lock`. Aikido is broader
  in scope but slower; both are useful.

## What we explicitly do not commit to

- **Promoting Aikido's check to required on the main-branch
  ruleset.** A third-party SaaS being down would block merges.
  `continue-on-error: true` keeps it advisory until proven useful
  over a release cycle. If at that point the false-positive rate is
  acceptable, we can flip the required-status setting in the
  ruleset.
- **Sharing the Aikido secret with forks.** The workflow's `if:`
  clause excludes `pull_request` events from forks because
  GitHub does not pass repo secrets to fork workflows; the scan
  would error noisily. Operator workflow: fork contributors push
  to a branch in this repo (or the maintainer pushes for them)
  before the scan can apply.
- **Pinning the upstream action by tag.** Tags are mutable; the
  workflow pins both `actions/checkout` and the Aikido action by
  immutable commit SHA, with the matching tag in a comment for
  readability. This is the standard supply-chain-hygiene pattern
  and is especially important for a security scanner — a
  repositioned tag on the scanner itself would let an attacker
  inject code into our CI with `pull-requests: write` permission
  and access to `AIKIDO_SECRET_KEY`.

## Revisit triggers

This decision should be revisited if:

- Aikido changes its free-tier policy in a way that affects
  open-source projects.
- We hit a sustained false-positive rate above a threshold the
  operator considers signal-killing.
- A different scanner (Snyk, Trivy, OSV-Scanner) becomes
  meaningfully better at the gap Aikido fills today.
- The required-status promotion question comes up again — at
  that point we either accept the SaaS dependency or replace it
  with something with offline scanning (Trivy, OSV-Scanner).
