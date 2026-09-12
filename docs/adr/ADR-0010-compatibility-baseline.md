# ADR-0010: POSIX Baseline Plus Explicit Bash Extensions

## Status
Accepted (baseline). Amended 2026-09-12 with a differential-testing gate.

## Context
"Bash-compatible" is too vague without further specification.

## Decision
The normative baseline is the POSIX Shell Command Language. Beyond that, Bash extensions are adopted explicitly per profile and feature flag.

Compatibility is now **empirically anchored**: a declared-compatible behaviour
must be backed by a differential case (`tests/suite/differential/*.toml` with
`[oracle] compare = true`) that runs the same input through wasmsh and a real
`bash` and asserts identical stdout, stderr, and exit status. A missing
reference shell makes the case a **visible SKIP**, never a silent pass.

## Consequences
- Clearer roadmap
- Fewer implicit compatibility promises
- Better test and documentation structure
- "Verified" is falsifiable: the `oracle` CI job fails on any behavioural
  drift from the bundled Bash, and `SUPPORTED.md` distinguishes
  differentially-verified features from known divergences and stubs.
