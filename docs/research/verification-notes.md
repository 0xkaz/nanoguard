> **Status:** shipped (process note; evergreen, last revised 2026-05-10)

# Verification Notes

This note documents how to evaluate contributions from other agents and
how to keep implementation work from drifting away from the current design.

## Evaluation Method

Use the following checklist for each contribution:

1. claim
2. evidence
3. failure mode
4. test coverage
5. docs sync

## What Counts as Evidence

Evidence should include at least one of:

- code path
- unit test
- integration test
- benchmark
- README or design doc update

If a claim cannot be tied back to one of those, it is still a hypothesis.

## Failure Modes To Check

- silent fallback behavior
- stale defaults
- untested edge cases
- mismatched docs
- behavior that depends on hidden configuration
- performance claims without benchmark context

## Benchmark Rules

When comparing alternatives, measure:

- build time
- clean input
- blocked input
- long clean input
- real dictionary set
- streaming path when relevant

The benchmark should mirror the deployed path, not a toy microbenchmark.

## Documentation Rule

If a change affects behavior, the corresponding `docs/design/*.md` entry
should describe it.

If a change informs a tradeoff or implementation choice, a
`docs/research/*.md` note should explain it.

## Practical Review Rule

Prefer "does this behave the way the docs say?" over "does the code look neat?"

That is the only review question that matters for a proxy with policy logic.

