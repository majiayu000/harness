# Tech Spec

## Linked Issue

GH-1673

## Product Spec

See `specs/GH1673/product.md`.

## Current System

On base `5a0a71b4755ccaac9c22f06949db117ffcb2beed`,
`crates/harness-workflow/src/runtime/worker.rs` contains 802 lines. Lines 1-522
own the production implementation, including runtime job execution, claim
guards, lease renewal, child-completion propagation, activity result handling,
turn budgets, runtime profiles, and failure metadata side effects. Lines
524-802 contain `#[cfg(test)] mod tests`, with five tests.

The test module already imports its parent with `use super::*`, so it is a
natural Rust child module that does not require inline physical ownership.

## Proposed Design

Keep `runtime/worker.rs` as the stable production module. Replace the inline
block with:

```rust
#[cfg(test)]
mod tests;
```

Move only the contents inside the existing `mod tests { ... }` block into
`crates/harness-workflow/src/runtime/worker/tests.rs`. The extracted file
begins with the existing `use super::*;` import and retains every local import,
fixture type, trait implementation, helper, and test in its current order.

Rust resolves the child file under the `runtime/worker/` directory while
preserving the logical module path `runtime::worker::tests`. Child-module
privacy continues to allow tests to access parent-private worker types and
helpers, so no production visibility or path change is required.

## Invariants and Inventory

- Record the exact base SHA, root line count, production item list, ordered
  five-test list, helper/type list, and test attributes before editing.
- After formatting, compare lines 1-522 of `worker.rs` byte-for-byte against
  the exact base and compare the complete production inventory.
- Compare the ordered helper/type/test/attribute inventory across the root and
  `runtime/worker/tests.rs` against the base; require every item exactly once.
- Review the moved test content as a pure relocation. Permitted edits are
  limited to replacing the inline wrapper with `mod tests;`, removing one
  indentation level from the moved block, and accepting deterministic line
  wrapping produced when the repository `rustfmt` version formats that
  unindented baseline body. No non-whitespace Rust token may change.
- Generate the expected child file by extracting exact-base lines 526-801,
  removing one four-space indentation level, and piping the result through
  `rustfmt --edition 2021 --emit stdout`; require an exact diff against
  `runtime/worker/tests.rs`.
- Preserve the three `#[tokio::test]` and two `#[test]` attributes, database
  resolver gates, local executor/guard implementations, job fixtures,
  assertions, JSON values, strings, store calls, and helper calls.
- Preserve exact `runtime::worker::tests::*` paths and require both Rust files
  to contain no more than 800 formatted lines.

## Affected Files

- `crates/harness-workflow/src/runtime/worker.rs`
- `crates/harness-workflow/src/runtime/worker/tests.rs`

No other source, test, manifest, lockfile, configuration, persistence, schema,
event, or serialized-format file is expected to change.

## Data Flow

Production runtime jobs, commands, events, leases, workflow instances,
activity results, database connections, and external calls do not change. Only
Rust test source layout changes at compile time under `cfg(test)`.

## Alternatives Considered

- Leave the inline tests in place: rejected because the file remains above the
  hard ceiling and mixes a bounded production implementation with 279 lines of
  runtime worker tests.
- Move tests to a top-level `runtime_worker_tests.rs` with a path attribute:
  rejected because standard Rust child-module layout preserves the existing
  namespace without an explicit path override.
- Split the five tests across several child modules: rejected because the
  extracted test module remains well below the ceiling and the tests share
  executor, guard, job, and workflow fixtures.
- Refactor production worker logic at the same time: rejected because it would
  expand scope beyond the safe test relocation.

## Risks

- Security: low; production execution, persistence, and input handling remain
  byte-for-byte unchanged.
- Compatibility: low; the logical test module path remains unchanged.
- Performance: none expected; production builds do not compile the test child
  module and test behavior is unchanged.
- Test integrity: low if the move matches the canonically formatted baseline,
  but inventory and movement-aware diff checks are mandatory to prevent a lost
  test, helper, attribute, assertion, or database setup path.
- Maintenance: reduced by separating runtime worker implementation from its
  focused verification while retaining direct child-module access.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-workflow --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-workflow runtime::worker::tests -- --test-threads=1`
      against a dedicated PostgreSQL database, with all five tests passing.
- [ ] Final `cargo test -p harness-workflow --lib` against the repository
      database test environment.
- [ ] `cargo fmt --all -- --check` and formatted line-count gates.
- [ ] Exact production and ordered test/helper inventory comparisons.
- [ ] Exact production diff plus canonical-rustfmt movement comparison proving
      no non-whitespace test token or production rewrite.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Workspace tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, VibeGuard compliance, exact-head CI,
      review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
