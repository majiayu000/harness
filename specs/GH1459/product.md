# Product Spec

## Linked Issue

GH-1459

## User Problem

Harness dev and test builds use Cargo's default full debug information because
the workspace has no development profile override. With many test binaries,
full debuginfo increases link time and grows `target/debug`, slowing local
agent loops, hooks, and CI.

Developers need backtraces and file-line information for failures, but the
repository does not require full interactive debugger variable inspection for
normal agent-driven builds.

## Goals

- Reduce dev and test build debug-info cost by setting the workspace dev
  profile to line-table debug information.
- Preserve panic backtraces with useful file and line information.
- Decide explicitly whether dependency debuginfo should be stripped in the same
  implementation PR.
- Record the target directory size impact after a clean rebuild.
- Coordinate implementation timing with GH-1458 so the build-profile and lint
  universe changes are paid in one intended rebuild where practical.

## Non-Goals

- Changing `profile.release` or release artifact behavior.
- Changing warning policy or `RUSTFLAGS`; GH-1458 owns that behavior.
- Reworking hook scope or CI job selection; GH-1464 and GH-1454 own those
  workflow changes.
- Optimizing individual slow tests.

## User-Visible Behavior

Normal dev and test builds produce smaller debug artifacts while still
preserving line-level panic diagnostics. A failing test backtrace should still
show source file and line entries. Interactive debugger variable inspection may
be less complete than with full debuginfo; that tradeoff is accepted for the
default repository profile.

If dependency debuginfo is stripped, the PR must call out that dependency stack
frames may have less detail while workspace crate line information remains the
primary debugging surface.

## Acceptance Criteria

- [ ] The root manifest defines `[profile.dev] debug = "line-tables-only"`.
- [ ] The implementation PR explicitly decides whether to add
      `[profile.dev.package."*"] debug = false`, with rationale in the PR body.
- [ ] `cargo test --workspace --lib` passes with the repository's existing
      Postgres-gated behavior.
- [ ] A temporary forced panic or failing test confirms panic output still
      includes file and line information, and the temporary change is reverted.
- [ ] The PR records before/after `du -sh target/debug` evidence after a clean
      rebuild or explains why the local measurement could not be collected.
- [ ] If GH-1458 lands in the same PR, the verification report separates lint
      fingerprint evidence from debug-info size evidence.

## Edge Cases

- `profile.test` inherits from `profile.dev`, so test binaries should benefit
  without a separate test profile unless implementation evidence shows a
  different setting is needed.
- A developer needing full debuginfo for a local debugging session can override
  locally; the repository default remains optimized for repeated agent and CI
  loops.
- Clean rebuild size measurements are machine- and cache-dependent, so the PR
  should record the command and local context rather than treating the number
  as a universal benchmark.
- Postgres-dependent tests still require `HARNESS_DATABASE_URL`; missing local
  Postgres should be reported as a verification limitation, not hidden.

## Rollout Notes

This is a build-profile change only. It should be implemented with GH-1458 or
immediately adjacent to it so maintainers understand the one-time rebuild cost
and the separate benefits: one compile fingerprint universe from GH-1458 and
smaller dev/test artifacts from GH-1459.
