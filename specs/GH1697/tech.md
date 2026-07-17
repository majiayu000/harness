# Tech Spec

## Linked Issue

GH-1697

## Product Spec

See `specs/GH1697/product.md`.

## Current System

On base `08157897b686c6ae9245d626c7d2997b93acdf27`,
`crates/harness-workflow/src/runtime/tests/runtime_store.rs` is a 900-line
module with 11 `#[tokio::test]` functions and 43 assertion/expectation
occurrences. Lines 620-832 are a contiguous block of nine helper functions;
line 833 is the blank separator before the final test.

## Proposed Design

Create
`crates/harness-workflow/src/runtime/tests/runtime_store_support.rs` from
exact-base lines 620-832.

Replace those lines in `runtime_store.rs` with:

```rust
include!("runtime_store_support.rs");
```

Retain exact-base lines 1-619 and 833-900 around the include. The include
expands inside the existing `runtime::tests::runtime_store` module, so imports,
visibility, helper calls, and logical test paths do not change.

## Helper Inventory

1. `runtime_job_status_str`
2. `insert_legacy_orphan_command`
3. `insert_runtime_job_record`
4. `update_runtime_job_record`
5. `assert_constraint_error`
6. `drop_runtime_graph_fk_constraints`
7. `insert_legacy_orphan_runtime_graph_rows`
8. `insert_runtime_job_with_status`
9. `test_runtime_job_command_id`

## Invariants

- Capture the exact base SHA, 900-line count, ordered 11-test inventory, nine
  helpers, 43 assertion/expectation occurrences, and compiled test list before
  editing.
- Require exact support and reconstructed-main comparisons.
- Preserve all source bytes except replacing the helper block with the include.
- Require all compiled test paths to match the baseline exactly.
- Require only the approved two files to change.

## Affected Files

- `crates/harness-workflow/src/runtime/tests/runtime_store.rs`
- `crates/harness-workflow/src/runtime/tests/runtime_store_support.rs`

## Alternatives Considered

- Leave the file unchanged: rejected because it remains above the hard ceiling.
- Split behavior tests into a nested module: rejected because it changes
  logical test paths.
- Move the final dedupe test to another include: rejected as unnecessary
  fragmentation; extracting the cohesive support block already reduces the
  main module to 688 lines.
- Refactor helpers or SQL: rejected as behavioral scope expansion.

## Risks

- Security and production risk: none expected; production and SQL source are
  unchanged.
- Test discovery risk: low; the support file is included in the same module.
- Test integrity risk: low when exact source, inventories, compiled paths, and
  full CI pass.

## Test Plan

Before editing:

```bash
BASE="$(git rev-parse HEAD)"
SOURCE=crates/harness-workflow/src/runtime/tests/runtime_store.rs
SUPPORT=crates/harness-workflow/src/runtime/tests/runtime_store_support.rs
BASE_LIST=/tmp/GH1697-base-tests.txt

test ! -e "$SUPPORT"
test "$(wc -l < "$SOURCE" | tr -d ' ')" = 900
test "$(rg -c '^#\[tokio::test\]' "$SOURCE")" = 11
test "$(rg -o 'assert(_eq|_ne)?!|expect\(' "$SOURCE" | wc -l | tr -d ' ')" = 43
cargo test -p harness-workflow --lib -- --list > "$BASE_LIST"
```

After editing:

```bash
FINAL_LIST=/tmp/GH1697-final-tests.txt

diff -u \
  <(git show "$BASE:$SOURCE" | sed -n '620,832p') \
  "$SUPPORT"
diff -u \
  <(git show "$BASE:$SOURCE" | awk '
    NR == 620 { print "include!(\"runtime_store_support.rs\");" }
    NR < 620 || NR >= 833 { print }
  ') \
  "$SOURCE"
test "$(wc -l < "$SOURCE" | tr -d ' ')" = 688
test "$(wc -l < "$SUPPORT" | tr -d ' ')" = 213
test "$(rg -c '^#\[tokio::test\]' "$SOURCE" "$SUPPORT" |
  awk -F: '{ total += $2 } END { print total }')" = 11
test "$(rg -o 'assert(_eq|_ne)?!|expect\(' "$SOURCE" "$SUPPORT" |
  wc -l | tr -d ' ')" = 43
test "$(sed -nE 's/^(async )?fn ([A-Za-z0-9_]+)\(.*/\2/p' "$SUPPORT" |
  wc -l | tr -d ' ')" = 9
cargo test -p harness-workflow --lib -- --list > "$FINAL_LIST"
diff -u "$BASE_LIST" "$FINAL_LIST"
diff -u \
  <(printf '%s\n' \
    crates/harness-workflow/src/runtime/tests/runtime_store.rs \
    crates/harness-workflow/src/runtime/tests/runtime_store_support.rs) \
  <(git diff --name-only "$BASE"...HEAD | sort)
```

Also run:

- `cargo fmt --all -- --check`
- `git diff --check`
- `cargo check -p harness-workflow --all-targets`
- `cargo test -p harness-workflow --lib runtime::tests::runtime_store::`
- database-independent full `cargo test -p harness-workflow --lib`
- `cargo clippy --workspace --all-targets -- -D warnings`
- repository pre-push tests, SpecRail checks, manual VibeGuard L1-L7 review,
  exact-head CI, review-thread audit, independent review, and required PR gate.

## Rollback Plan

Inline the exact support-file source at the include and delete the support
file, or squash-revert the implementation PR.
