# Reviewability standard

- **Status:** Active
- **Applies to:** Maintained source code and reviewable changes
- **Nature:** Advisory thresholds with required judgment, not hard code limits

## Purpose

Review size affects defect detection, ownership and rollback safety, but lines
of code do not measure architecture quality. These thresholds identify work
that deserves an explicit reviewability decision. Exceeding one does not fail a
build or prove that code is poorly designed.

Hard controls remain the invariants in `AGENTS.md`, architecture tests, compiler
checks, behavior tests, security boundaries and runtime resource limits.

## Review thresholds

| Signal | Review threshold | Required response when exceeded |
|---|---:|---|
| Function or method | More than 75 logical lines | Check whether it combines policy, orchestration and mechanics; extract only cohesive responsibilities |
| Maintained source file | More than 600 physical lines | Check whether the file contains more than one ownership reason to change |
| Change size | More than 500 added plus deleted source lines | Explain why the change remains independently reviewable or split it by behavior/decision |
| Change breadth | More than 20 changed files | Identify mechanical versus semantic files and describe dependency/order of review |

“Source” includes Rust, JavaScript/TypeScript, CSS and HTML maintained by this
repository. Documentation is reviewed for clarity but is not counted as source
size for these thresholds.

## Exceptions

The following are excluded when they are clearly identified in the change:

- generated code and generated bindings;
- vendored third-party code;
- test fixtures, protocol corpora and snapshots;
- database migration files that must be atomic;
- lockfiles and generated dependency metadata;
- mechanical renames or formatting-only changes;
- a single cohesive lookup table or declarative schema that becomes less clear
  when fragmented.

An exception is not permission to mix unrelated behavior. The PR records the
excluded files and why reviewers can treat them mechanically.

## Review procedure

1. Run `node scripts/review-size-report.mjs` to identify maintained files above
   the source-file threshold.
2. For a branch comparison, run
   `node scripts/review-size-report.mjs --base <target-ref>`.
3. Complete the Reviewability section of the PR template.
4. If a threshold is exceeded, choose one outcome:
   - split by an independently testable behavior or architectural decision;
   - extract a genuinely cohesive module/function;
   - retain the shape and explain why splitting would reduce clarity or safety.
5. Review architecture, security, durability and rollback risk regardless of
   whether the change is below every threshold.

## Current baseline debt

Existing files above 600 lines are not automatically scheduled for splitting.
They become candidates when touched for material behavior, and any split must
preserve public contracts and be independently verified. The advisory report
keeps this visible without turning historical size into a failing gate.

## Prohibited uses

- Do not fail CI solely because a function, file or change crosses a threshold.
- Do not introduce pass-through modules, arbitrary “part” files or fragmented
  functions solely to reduce counts.
- Do not approve a small change that violates module ownership or an invariant.
- Do not use excluded/generated lines to conceal unrelated semantic changes.
