# Tech Spec

## Linked Issue

GH-1462

## Product Spec

See `specs/GH1462/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Runtime test root | `crates/harness-workflow/src/runtime/tests.rs` | 5,554 lines on current main; contains root imports, shared fixtures, decision-builder tests, reducer tests, validator tests, worker tests, store tests, and dispatcher tests. | This is the remaining oversized surface to split. |
| Existing split modules | `crates/harness-workflow/src/runtime/tests/*.rs` | Several domains already live in child modules and import root helpers with `use super::*`. | The implementation should continue this pattern or replace it with a common helper module. |
| Workflow runtime code | `crates/harness-workflow/src/runtime/{model,store,validator,reducer,...}.rs` | Production runtime behavior is already covered by the tests being moved. | These files should not change for this issue. |
| Agent file-size policy | `CLAUDE.md` | Still grants `**/harness-workflow/src/runtime/tests.rs` a 6,500-line U-16 exemption. | The exemption must be removed or reduced once the split lands. |

## Proposed Design

1. Capture the pre-split test inventory.
   - Run `cargo test -p harness-workflow -- --list`.
   - Save sorted test names to an untracked artifact for local comparison.
2. Create a common test support module.
   - Add `crates/harness-workflow/src/runtime/tests/common.rs`.
   - Move shared imports, workflow instance constructors, job enqueue helpers,
     event builders, validation context helpers, and simple test executors into
     `common.rs`.
   - Re-export only test support needed by sibling modules.
3. Split the remaining root tests by domain.
   - Suggested files:
     - `decision_builders.rs` for issue/prompt/plan/PR feedback/quality-gate
       decision construction tests.
     - `completion_reducer.rs` for `reduce_runtime_job_completed` tests.
     - `decision_validator.rs` for `DecisionValidator` tests.
     - `worker_lifecycle.rs` for runtime worker and child-propagation tests.
     - `command_dispatcher.rs` for `RuntimeCommandDispatcher` tests.
     - `command_store.rs` or equivalent for remaining command/runtime-store
       behavior not already covered by `runtime_store.rs`.
   - Preserve existing child modules such as `issue_planning`,
     `pr_repair_evidence`, `retry`, and `runtime_store`.
4. Keep the root module thin.
   - `runtime/tests.rs` should declare modules and import/re-export shared test
     support only when needed.
   - Avoid duplicating large import lists in each file when `common.rs` can own
     them.
5. Update file-size policy.
   - Remove `**/harness-workflow/src/runtime/tests.rs` from `CLAUDE.md`, or
     replace it with a much lower ceiling that matches the new thin root.
6. Verify inventory and behavior.
   - Run the pre/post sorted test-name diff.
   - Run `cargo test -p harness-workflow`.
   - Run formatting and clippy gates.

## Data Flow

No runtime data flow changes. Tests continue to construct workflow instances,
commands, jobs, activity results, validation contexts, and database-backed
stores exactly as before; only their source file locations change.

## Alternatives Considered

- Leave the root file partially split. Rejected because it remains far above the
  normal U-16 ceiling and still requires a 6,500-line exemption.
- Split by chronological order only. Rejected because domain-focused files are
  easier to navigate and align with existing runtime concepts.
- Rewrite helpers while moving tests. Rejected because the issue requires a
  mechanical split and test-integrity protection.
- Use generated include files. Rejected because normal Rust modules are easier
  for agents and maintainers to search, review, and edit.

## Risks

- Test names can change if modules are renamed carelessly. Mitigate with sorted
  `--list` comparison.
- Helper visibility can accidentally broaden or break imports. Mitigate by
  keeping helpers in a private test-only module and compiling with the full
  package test.
- Moving async DB tests can hide local failures when Postgres is unavailable.
  Mitigate by preserving existing `resolve_database_url(None)` skip checks and
  relying on CI DB coverage.
- Large mechanical diffs can hide behavioral edits. Mitigate with move-only
  review, inventory diff, and focused package tests.

## Test Plan

- [ ] Capture pre-split sorted inventory:
      `cargo test -p harness-workflow -- --list`.
- [ ] Capture post-split sorted inventory and verify the diff is empty.
- [ ] Run `cargo test -p harness-workflow`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1462`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.

## Rollback Plan

Revert the split commit. Because the implementation should only move tests and
update the U-16 exemption, rollback restores the previous file layout without a
data migration or production behavior change.
