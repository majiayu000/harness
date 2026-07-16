# Tech Spec

## Linked Issue

GH-1680

## Product Spec

See `specs/GH1680/product.md`.

## Current System

On base `968d0058d852b9ef44bad37a3527938f3852e812`,
`crates/harness-server/src/http/tests/runtime_worker_tests.rs` contains 822
lines and seven `#[tokio::test]` functions. Lines 1-409 contain three tests for
registered-agent execution, terminal-workspace cleanup, and already-terminal
workflow cancellation. Lines 410-822 contain four cohesive tests that start
generic prompt, open-PR prompt-feedback, PR-feedback, and quality-gate child
workflows without an agent turn.

The root test module imports its parent with `use super::*`. A nested child
module can import the root test namespace the same way without changing
production visibility.

## Proposed Design

Keep exact-base lines 1-409 in `runtime_worker_tests.rs`, then append:

```rust
mod child_workflows;
```

Create
`crates/harness-server/src/http/tests/runtime_worker_tests/child_workflows.rs`
with `use super::*;`, one blank line, and exact-base lines 410-822 in their
current order.

The three retained tests keep their current logical paths. The four moved
tests intentionally become
`http::tests::runtime_worker_tests::child_workflows::<test_name>`. The existing
parent filter continues to select all seven tests.

## Invariants and Inventory

- Record the exact base SHA, root line count, ordered seven-test list,
  attributes, and split boundary before editing.
- Require final root lines 1-409 to match the exact base byte-for-byte and the
  only appended root content to be `mod child_workflows;`.
- Generate the expected child file from `use super::*;`, a blank line, and
  exact-base lines 410-822; require an exact diff after repository `rustfmt`.
- Preserve all seven `#[tokio::test]` attributes, names, result types, bodies,
  assertions, fixtures, workflow definitions, command payloads, database
  gates, store calls, strings, and non-whitespace Rust tokens exactly once.
- Require the three retained paths to remain unchanged and the four moved paths
  to differ only by the new `child_workflows` segment.
- Require both formatted files to contain no more than 800 lines and no file
  outside the approved pair to change.

## Affected Files

- `crates/harness-server/src/http/tests/runtime_worker_tests.rs`
- `crates/harness-server/src/http/tests/runtime_worker_tests/child_workflows.rs`

No production source, other test, manifest, lockfile, configuration,
persistence, schema, or serialized-format file is expected to change.

## Data Flow

Production runtime jobs, workflow instances, commands, child workflows, agent
turns, databases, and HTTP requests do not change. Only Rust test module layout
changes under the existing test-only namespace.

## Alternatives Considered

- Leave the file unchanged: rejected because it remains above the hard ceiling
  and mixes two distinct runtime-worker test concerns.
- Move only the final quality-gate test: rejected because the four tests share
  one bounded child-workflow-start concern and fit comfortably together.
- Split every test into a separate module: rejected because it fragments a
  seven-test suite without improving cohesion.
- Refactor shared fixtures or production code simultaneously: rejected because
  it expands scope beyond a safe mechanical relocation.

## Risks

- Security: low; production source, input handling, authentication, process
  execution, and persistence implementation are unchanged.
- Compatibility: low; only four test paths gain one documented module segment,
  while the stable parent filter still selects the full suite.
- Performance: none expected outside negligible test compilation layout.
- Test integrity: low if the exact retained/moved comparisons pass; inventory
  and PostgreSQL execution remain mandatory.
- Maintenance: improved by separating execution/terminal-state coverage from
  child-workflow-start coverage.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-server --all-targets`.
- [ ] Baseline and final PostgreSQL-backed
      `cargo test -p harness-server http::tests::runtime_worker_tests -- --test-threads=1`,
      with all seven tests passing.
- [ ] Final `cargo test -p harness-server --lib` against a dedicated database.
- [ ] `cargo fmt --all -- --check`, line-count, exact root, exact child,
      ordered inventory, namespace, and two-file scope checks.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and workspace
      tests under the repository test environment.
- [ ] SpecRail global/packet/route checks, manual VibeGuard L1-L7 review,
      exact-head CI, review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration,
configuration, dependency, or operator rollback step is required.
