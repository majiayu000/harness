# Tech Spec

## Linked Issue

GH-1659

## Product Spec

See `specs/GH1659/product.md`.

## Current System

On base `849d416195dbfa5caabbba2ac8c3c4d3975f39c9`,
`crates/harness-gc/src/gc_agent.rs` contains 1,047 lines. Lines 1-626 own the
production implementation, including two public types and the GC run,
adoption, checkpoint, path-validation, prompt, and artifact-parsing functions.
Lines 627-1,047 contain `#[cfg(test)] mod tests`, with three private fixtures
and 15 tests.

The test module already imports its parent with `use super::*`, so it is a
natural Rust child module that does not require inline physical ownership.

## Proposed Design

Keep `gc_agent.rs` as the stable production module. Replace the inline block
with:

```rust
#[cfg(test)]
mod tests;
```

Move only the contents inside the existing `mod tests { ... }` block into
`crates/harness-gc/src/gc_agent/tests.rs`. The extracted file begins with the
existing `use super::*;` import and retains every fixture and test in its
current order.

Rust resolves the child file under the `gc_agent/` directory while preserving
the logical module path `gc_agent::tests`. Child-module privacy continues to
allow tests to access parent-private helpers, so no production visibility or
path change is required.

## Invariants and Inventory

- Record the exact base SHA, root line count, production function/type list,
  fixture list, test-name list, attributes, and focused test output before
  editing.
- After formatting, compare the production inventory in `gc_agent.rs` exactly
  against the base.
- Compare the ordered test-name and fixture inventory across the root plus
  `gc_agent/tests.rs` against the base; require every item exactly once.
- Review the moved test content as a pure relocation. Permitted textual edits
  are limited to replacing the inline wrapper with `mod tests;` and removing
  one indentation level from the moved block.
- Preserve all attributes, strings, assertions, temporary-directory setup,
  error matching, and helper calls.
- Require both Rust files to contain no more than 800 formatted lines.

## Affected Files

- `crates/harness-gc/src/gc_agent.rs`
- `crates/harness-gc/src/gc_agent/tests.rs`

No other source, test, manifest, lockfile, configuration, schema, or persisted
format file is expected to change.

## Alternatives Considered

- Leave the inline tests in place: rejected because the file remains above the
  hard ceiling and mixes a bounded production implementation with 421 lines of
  tests.
- Move tests to a top-level `gc_agent_tests.rs` with a path attribute: rejected
  because Rust standard child-module layout preserves the existing namespace
  without an explicit path override.
- Split tests into several files: rejected because 421 lines are below the
  ceiling and the 15 tests form one cohesive GC-agent suite.
- Refactor artifact parsing or adoption into new production modules at the
  same time: rejected because that would expand scope beyond the safe test
  relocation.

## Risks

- Compatibility: low; the logical test module path remains unchanged.
- Production behavior: minimal; no production body or visibility changes.
- Test integrity: low if the move is exact, but inventory and diff checks are
  mandatory to prevent a lost test, assertion, attribute, or fixture.
- Security: low; no auth, secret, command, network, or permission path changes.
- Maintenance: reduced by separating production implementation from its test
  suite while retaining direct child-module access.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-gc --all-targets`.
- [ ] Baseline and final `cargo test -p harness-gc gc_agent` with all 15 tests.
- [ ] Final `cargo test -p harness-gc`.
- [ ] `cargo fmt --all -- --check` and formatted line-count gates.
- [ ] Exact production and test inventory comparisons.
- [ ] Diff scope and movement review proving no test or production rewrite.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Workspace tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, VibeGuard compliance, exact-head CI,
      review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, configuration, dependency, or
operator rollback step is required.
