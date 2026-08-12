---
title: "Review overview and severity"
description: "The full 43-finding correctness and performance review, its severity model, stable identifiers, and reproduced regressions."
sidebar:
  order: 1
---
The implementation review records **43 independently identified
findings**: **30 correctness and compatibility issues** and
**13 performance, capacity, or workflow issues**. Severity is ordered
by the potential for data loss, permanently incorrect repository state, broad
stock-behavior regressions, and resource exhaustion.

| Severity | Meaning | Correctness findings | Performance findings |
| --- | --- | --- | --- |
| **P0** | Release-blocking data loss, falsely clean state, broken builds, or unbounded system-wide cost. | [Read P0 correctness](/review/p0-correctness/) | [Read P0 performance](/review/p0-performance/) |
| **P1** | Substantial correctness, compatibility, isolation, or scaling defects. | [Read P1 correctness](/review/p1-correctness/) | [Read P1 performance](/review/p1-performance/) |
| **P2** | Remaining compatibility gaps and recurring avoidable overhead. | [Read P2 correctness](/review/p2-correctness/) | [Read P2 performance](/review/p2-performance/) |

The identifiers are stable across this documentation site and `SPEC.md`:
`C-01` through `C-30` identify correctness findings, while `P-01` through
`P-13` identify performance or operational findings.

## What the review verified

The analysis traces the current AWACS implementation and the companion
Jujutsu checkout, distinguishes running code from unused scaffolding, and
compares modified Jujutsu behavior with both stock Jujutsu and Git.

The [current remediation tracker](/reference/current-fixes/) groups those
findings into actionable implementation work; the
[implementation roadmap](/reference/implementation-roadmap/) records the
broader development plan.

The most serious reproduced regressions include shared repository deletion,
automatic-snapshot fallback recording tracked files as deleted, destruction of
unsnapshotted sibling edits, symlink-target deletion, and reversed global-ignore
precedence. The [validation and acceptance gates](/operations/validation/)
record the review-time evidence and the scenarios required before adoption.

## Previously resolved reports

An older audit includes issues that have since been corrected. Keep those
[resolved findings](/review/resolved-findings/) separate from the active list.
