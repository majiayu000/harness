# Tech Spec

## Linked Issue

GH-1677

## Product Spec

See `specs/GH1677/product.md`.

## Current System

On base `4db3d0d4842cebb25c4c69519acdd386e154a8e1`,
`crates/harness-core/src/config/intake.rs` contains 803 lines. Lines 1-491 own
the first production section. Lines 492-792 contain `#[cfg(test)] mod tests`,
with 12 tests. Lines 793-803 resume production ownership with the complete
`IntakeConfig` type.

The test module already imports its parent with `use super::*`, so it is a
natural Rust child module that does not require inline physical ownership.

## Proposed Design

Keep `config/intake.rs` as the stable production module. Replace the inline
block with:

```rust
#[cfg(test)]
mod tests;
```

Move only the contents inside the existing `mod tests { ... }` block into
`crates/harness-core/src/config/intake/tests.rs`. The extracted file begins
with the existing `use super::*;` import and retains every test and local value
in its current order.

Rust resolves the child file under the `config/intake/` directory while
preserving the logical module path `config::intake::tests`. Child-module
privacy continues to allow tests to access parent-private constants and
helpers, so no production visibility or path change is required.

## Invariants and Inventory

- Record the exact base SHA, root line count, production item list, ordered
  12-test list, and test attributes before editing.
- After formatting, compare exact-base lines 1-491 and 793-803 byte-for-byte
  against their corresponding final production sections and compare the
  complete production inventory.
- Compare the ordered test/attribute inventory across the root and
  `config/intake/tests.rs` against the base; require every item exactly once.
- Review the moved test content as a pure relocation. Permitted edits are
  limited to replacing the inline wrapper with `mod tests;`, removing one
  indentation level from the moved block, and accepting deterministic line
  wrapping produced when the repository `rustfmt` version formats that
  unindented baseline body. No non-whitespace Rust token may change.
- Generate the expected child file by extracting exact-base lines 494-791,
  removing one four-space indentation level, and piping the result through
  `rustfmt --edition 2021 --emit stdout`; require an exact diff against
  `config/intake/tests.rs`.
- Preserve all 12 `#[test]` attributes, TOML fixtures, result types, comments,
  assertions, strings, policy values, validation calls, and redaction checks.
- Preserve exact `config::intake::tests::*` paths and require both Rust files
  to contain no more than 800 formatted lines.

## Affected Files

- `crates/harness-core/src/config/intake.rs`
- `crates/harness-core/src/config/intake/tests.rs`

No other source, test, manifest, lockfile, configuration, persistence, schema,
or serialized-format file is expected to change.

## Data Flow

Production intake configuration values, parsing, validation, serialization,
debug output, repository selection, merge policy, recovery policy, and
external calls do not change. Only Rust test source layout changes at compile
time under `cfg(test)`.

## Alternatives Considered

- Leave the inline tests in place: rejected because the file remains above the
  hard ceiling and mixes a bounded production implementation with 301 lines
  of intake configuration tests.
- Move tests to a top-level `intake_tests.rs` with a path attribute: rejected
  because standard Rust child-module layout preserves the existing namespace
  without an explicit path override.
- Split the 12 tests across several child modules: rejected because the
  extracted test module remains well below the ceiling and the tests share the
  same parent configuration types and constants.
- Refactor production configuration code at the same time: rejected because
  it would expand scope beyond the safe test relocation.

## Risks

- Security: low; production parsing, validation, redaction, persistence, and
  input handling remain byte-for-byte unchanged.
- Compatibility: low; the logical test module path remains unchanged.
- Performance: none expected; production builds do not compile the test child
  module and test behavior is unchanged.
- Test integrity: low if the move matches the canonically formatted baseline,
  but inventory and movement-aware diff checks are mandatory to prevent a lost
  test, assertion, fixture, comment, attribute, or redaction check.
- Maintenance: reduced by separating intake configuration implementation from
  its focused verification while retaining direct child-module access.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-core --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-core config::intake::tests`, with all 12 tests
      passing.
- [ ] Final `cargo test -p harness-core --lib`.
- [ ] `cargo fmt --all -- --check` and formatted line-count gates.
- [ ] Exact production and ordered test inventory comparisons.
- [ ] Exact production-section diffs plus canonical-rustfmt movement
      comparison proving no non-whitespace test token or production rewrite.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Workspace tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, VibeGuard compliance, exact-head CI,
      review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
