# Tech Spec

## Linked Issue

GH-1665

## Product Spec

See `specs/GH1665/product.md`.

## Current System

On base `25213f83e7448a6cf06ee46b20b956313833e567`,
`crates/harness-server/src/webhook.rs` contains 985 lines. Lines 1-333 own the
production implementation, including GitHub payload types, task-request
construction, event parsing, event-name validation, hexadecimal decoding, and
HMAC signature verification. Lines 334-985 contain `#[cfg(test)] mod tests`,
with 33 synchronous tests.

The test module already imports its parent with `use super::*`, so it is a
natural Rust child module that does not require inline physical ownership.

## Proposed Design

Keep `webhook.rs` as the stable production module. Replace the inline block
with:

```rust
#[cfg(test)]
mod tests;
```

Move only the contents inside the existing `mod tests { ... }` block into
`crates/harness-server/src/webhook/tests.rs`. The extracted file begins with
the existing `use super::*;` import and retains every test and local import in
its current order.

Rust resolves the child file under the `webhook/` directory while preserving
the logical module path `webhook::tests`. Child-module privacy continues to
allow tests to access parent-private payload types and helpers, so no
production visibility or path change is required.

## Invariants and Inventory

- Record the exact base SHA, root line count, production function/type list,
  and ordered 33-test list before editing.
- After formatting, compare lines 1-333 of `webhook.rs` byte-for-byte against
  the exact base and compare the complete production inventory.
- Compare the ordered test-name and attribute inventory across the root plus
  `webhook/tests.rs` against the base; require every test exactly once.
- Review the moved test content as a pure relocation. Permitted textual edits
  are limited to replacing the inline wrapper with `mod tests;` and removing
  one indentation level from the moved block.
- Preserve all JSON/HMAC fixtures, strings, assertions, local imports, request
  comparisons, ignored reasons, and helper calls.
- Preserve the exact `webhook::tests::*` paths and require both Rust files to
  contain no more than 800 formatted lines.

## Affected Files

- `crates/harness-server/src/webhook.rs`
- `crates/harness-server/src/webhook/tests.rs`

No other source, test, manifest, lockfile, configuration, schema, or persisted
format file is expected to change.

## Alternatives Considered

- Leave the inline tests in place: rejected because the file remains above the
  hard ceiling and mixes a bounded production implementation with 652 lines of
  tests.
- Move tests to a top-level `webhook_tests.rs` with a path attribute: rejected
  because standard Rust child-module layout preserves the existing namespace
  without an explicit path override.
- Split tests into event-specific files: rejected because the extracted module
  remains below the ceiling and the 33 tests form one cohesive parser suite.
- Refactor production event handlers at the same time: rejected because that
  would expand scope beyond the safe test relocation.

## Risks

- Compatibility: low; the logical test module path remains unchanged.
- Production behavior: minimal; no production body or visibility changes.
- Test integrity: low if the move is exact, but inventory and movement-aware
  diff checks are mandatory to prevent a lost test, assertion, fixture, or
  attribute.
- Security: low; production signature verification and event validation remain
  byte-for-byte unchanged.
- Maintenance: reduced by separating production webhook handling from its test
  suite while retaining direct child-module access.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-server --all-targets`.
- [ ] Baseline and final
      `cargo test -p harness-server webhook::tests -- --test-threads=1` with all
      33 tests.
- [ ] Final `cargo test -p harness-server --lib -- --test-threads=1` against the
      repository database test environment.
- [ ] `cargo fmt --all -- --check` and formatted line-count gates.
- [ ] Exact production and ordered test inventory comparisons.
- [ ] Diff-scope and movement review proving no test or production rewrite.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Workspace tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, VibeGuard compliance, exact-head CI,
      review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, configuration, dependency, or
operator rollback step is required.
