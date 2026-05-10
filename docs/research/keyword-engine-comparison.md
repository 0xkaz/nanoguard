> **Status:** shipped (commit 82d65f3, 2026-05-10)

# Keyword Engine Comparison

This note compares the two keyword engines currently supported by nanoguard:

- `iword-rs` legacy mode
- `aho-corasick` default mode

The goal is not to declare a universal winner.
The goal is to explain why `aho-corasick` became the default and when the
legacy engine is still useful.

## Summary

`aho-corasick` is the better default for nanoguard's current workload.

Reason:

- nanoguard mostly scans a small-to-medium set of literal policy phrases
- the project already normalizes input before matching
- the default deployment cares about request-path latency more than build-time
- the current dictionary format mixes literal rules with regex rules
- `aho-corasick` integrates cleanly with a separate regex path

`iword-rs` remains useful as a comparison point and compatibility fallback.

## What Each Engine Optimizes For

### `iword-rs`

The legacy engine is optimized around a compact word-search workflow with
project-specific dictionary handling. In nanoguard's current codebase and
documentation, it is treated as a rolling-hash-based keyword engine rather
than a finite-state automaton matcher.

It is attractive when:

- the rule set is small
- the matching semantics are tied to the existing dictionary format
- build simplicity matters more than absolute hot-path speed
- you want a legacy-compatible implementation while keeping the old behavior

### `aho-corasick`

`aho-corasick` is a mature multi-pattern matcher that builds a finite-state
machine for linear-time scans.

It is attractive when:

- you want a standard, well-known engine for many literals
- leftmost-first or leftmost-longest semantics matter
- you want to separate literal matching from regex matching
- you care about hot-path performance on repeated scans

## Operational Fit in nanoguard

nanoguard's actual rule stack looks like this:

1. normalize the text
2. scan literal BLOCK rules
3. scan literal ALERT rules
4. scan literal FLAG rules
5. evaluate regex-backed PII entities
6. redact, reject, or log depending on policy

That structure favors `aho-corasick` because:

- literal rules can be handled in one matcher
- regex rules can be evaluated separately
- the policy semantics live in nanoguard, not in the matcher crate

The old engine is still supported because it gives us a controlled fallback
and a way to compare behavior when rule normalization changes.

## Local Benchmark Snapshot

These are local measurements from the repository's `engine_compare` benchmark
on an Apple M-series machine.

### Inline rules only

| Case | iword-rs | aho-corasick |
|---|---:|---:|
| clean short input | ~10.4 us | ~111 ns |
| blocked short input | ~7.2 us | ~160 ns |
| long clean input (~2700 chars) | ~1.02 ms | ~5.0 us |

### Real dictionary set

| Case | iword-rs | aho-corasick |
|---|---:|---:|
| clean input | ~44 us | ~0.5 us |
| literal block | ~8.1 us | ~0.17 us |
| regex alert | ~249 us | ~0.28 us |
| regex block | ~323 us | ~0.20 us |
| flag match | ~11.2 us | ~0.20 us |
| long clean input (~2700 chars) | ~1.35 ms | ~65 us |

### Build time

| Case | iword-rs | aho-corasick |
|---|---:|---:|
| build with dict files | ~1.15 ms | ~2.5 ms |

Interpretation:

- `aho-corasick` is materially faster on the request path.
- `iword-rs` can still be competitive on setup cost.
- for a proxy, request-path speed matters more than a small build-time delta.

## Why the Legacy Engine Stays

The legacy engine is still worth keeping for now because:

- it makes regressions easier to spot
- it offers a fallback if matcher semantics need to be compared
- it helps validate that normalization and dictionary parsing stay stable
- it keeps the existing behavior path available while the new default settles

This is especially useful while the policy format is still evolving.

## Recommendation

Use `aho-corasick` as the default engine.
Keep `iword-rs` as an explicit compatibility mode until:

- the rule format stops changing
- the normalization rules stop changing
- enough deployments have exercised the new default

At that point, the project can decide whether the legacy path is still worth
the maintenance cost.
