# Tech Spec

## Linked Issue

GH-1670

## Product Spec

See `specs/GH1670/product.md`.

## Current System

On base `654751c3a83672bfbdcdf2c771c69a199a96de3f`,
`crates/harness-workflow/src/plan_db.rs` contains 822 lines. Lines 1-388 own
the production implementation, including migrations, `PlanDb`, CRUD/search
operations, transaction updates, markdown imports, shared-schema access, and
legacy backfill. Lines 389-822 contain `#[cfg(test)] mod tests`, with 15 tests.

The test module already imports its parent with `use super::*`, so it is a
natural Rust child module that does not require inline physical ownership.

## Proposed Design

Keep `plan_db.rs` as the stable production module. Replace the inline block
with:

```rust
#[cfg(test)]
mod tests;
```

Move only the contents inside the existing `mod tests { ... }` block into
`crates/harness-workflow/src/plan_db/tests.rs`. The extracted file begins with
the existing `use super::*;` import and retains every helper, import, static,
and test in its current order.

Rust resolves the child file under the `plan_db/` directory while preserving
the logical module path `plan_db::tests`. Child-module privacy continues to
allow tests to access parent-private constants, migrations, methods, and
helpers, so no production visibility or path change is required.

## Invariants and Inventory

- Record the exact base SHA, root line count, production item list, ordered
  15-test list, and test attributes before editing.
- After formatting, compare lines 1-388 of `plan_db.rs` byte-for-byte against
  the exact base and compare the complete production inventory.
- Compare the ordered helper/test/attribute inventory across the root plus
  `plan_db/tests.rs` against the base; require every test exactly once.
- Review the moved test content as a pure relocation. Permitted edits are
  limited to replacing the inline wrapper with `mod tests;`, removing one
  indentation level from the moved block, and accepting deterministic line
  wrapping produced when the repository `rustfmt` version formats that
  unindented baseline body. No non-whitespace Rust token may change.
- Generate the expected child file by extracting base lines 391-821, removing
  one four-space indentation level, and piping the result through
  `rustfmt --edition 2021 --emit stdout`; require an exact diff against
  `plan_db/tests.rs`.
- Preserve all migration arrays, SQL strings, schema construction, JSON and
  markdown fixtures, assertions, transaction calls, database gates, imports,
  and helper calls.
- Preserve the exact `plan_db::tests::*` paths and require both Rust files to
  contain no more than 800 formatted lines.

## Affected Files

- `crates/harness-workflow/src/plan_db.rs`
- `crates/harness-workflow/src/plan_db/tests.rs`

No other source, test, manifest, lockfile, configuration, migration, schema,
or persisted-format file is expected to change.

## Data Flow

Production inputs, outputs, database connections, SQL execution, persisted
plans, migrations, and external calls do not change. Only Rust test source
layout changes at compile time under `cfg(test)`.

## Alternatives Considered

- Leave the inline tests in place: rejected because the file remains above the
  hard ceiling and mixes a bounded production implementation with 434 lines of
  database tests.
- Move tests to a top-level `plan_db_tests.rs` with a path attribute: rejected
  because standard Rust child-module layout preserves the existing namespace
  without an explicit path override.
- Split tests by migration and CRUD behavior: rejected because the extracted
  test module remains below the ceiling and the 15 tests share database setup
  and serialization helpers.
- Refactor production persistence logic at the same time: rejected because
  that would expand scope beyond the safe test relocation.

## Risks

- Security: low; SQL, migrations, database URLs, and production persistence
  code remain byte-for-byte unchanged.
- Compatibility: low; the logical test module path remains unchanged.
- Performance: none expected; production builds do not compile the test child
  module and test behavior is unchanged.
- Test integrity: low if the move matches the canonically formatted baseline,
  but inventory and movement-aware diff checks are mandatory to prevent a lost
  test, non-whitespace token, SQL statement, assertion, fixture, or attribute.
- Maintenance: reduced by separating plan persistence implementation from its
  database verification while retaining direct child-module access.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-workflow --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-workflow plan_db::tests -- --test-threads=1`
      against a dedicated PostgreSQL database, with all 15 tests passing.
- [ ] Final `cargo test -p harness-workflow --lib` against the repository
      database test environment.
- [ ] `cargo fmt --all -- --check` and formatted line-count gates.
- [ ] Exact production and ordered test inventory comparisons.
- [ ] Exact production diff plus canonical-rustfmt movement comparison proving
      no non-whitespace test token, SQL, fixture, or production rewrite.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Workspace tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, VibeGuard compliance, exact-head CI,
      review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
