# Product Spec

## Linked Issue

GH-1443

## User Problem

The kernel contraction (#1434) plans to delete the legacy task layer, but the workflow runtime's live agent-turn engine currently lives *inside* that layer (`task_executor/turn_lifecycle.rs`, `task_executor/helpers.rs`). Deleting by module name would sever the hot path that executes every production runtime job. Maintainers need the module ownership to match the keep/delete boundary before any deletion PR lands.

## Goals

- The workflow runtime's turn engine lives under workflow-runtime ownership; `task_executor/` afterwards contains only legacy-path code.
- The live command-safety check is separated from the legacy post-execution validator.
- The move is purely mechanical (logic-preserving), so #1434 deletion PRs can subsequently remove whole directories.

## Non-Goals

- Deleting any code (GH-1442 and #1434 own deletions).
- Behavior changes to turn execution, stall/timeout semantics (GH-1437), or token-usage handling (GH-1439).
- Renaming config keys, RPC methods, or HTTP routes.

## Behavior Invariants

1. Every runtime job executes exactly as before: same prompts, same stream handling, same persistence, same failure strings.
2. All existing tests pass without assertion changes; only import paths in tests may change.
3. After the move, no file under `task_executor/` is referenced by `workflow_runtime_worker/` (the deletion boundary is clean).
4. `validate_command_safety` remains invoked from workspace setup with identical semantics.
5. The relocated modules carry a doc comment stating ownership and that `task_executor/` is legacy pending #1434.

## Acceptance Criteria

- [ ] `turn_lifecycle.rs` + `helpers.rs` (and direct-only private deps) moved under `workflow_runtime_worker/` (or a `turn_engine` module it owns); `git log --follow` shows pure moves.
- [ ] `validate_command_safety` relocated out of `post_validator`; legacy interceptor shell untouched otherwise.
- [ ] `rg "task_executor" crates/harness-server/src/workflow_runtime_worker/` returns nothing.
- [ ] `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` and `cargo test --workspace` green.

## Edge Cases

- Shared private helpers used by BOTH the hot files and legacy files: duplicate into the legacy side rather than keeping a cross-boundary import (the legacy copy dies with #1434).
- `thread_manager` calls from the moved code stay as-is (thread_manager survives contraction as in-memory state).
- Re-exports may be needed temporarily if external crates (CLI) import moved types — prefer updating importers over leaving re-export shims.

## Rollout Notes

No runtime behavior change; ships as one refactor PR before any #1434 deletion PR. Coordinate with in-flight branches touching `task_executor/` (feat/usage-monitor-dashboard's `activity_result.rs` is in `workflow_runtime_worker/`, unaffected).
