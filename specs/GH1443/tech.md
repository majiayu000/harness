# Tech Spec

## Linked Issue

GH-1443

## Product Spec

See `specs/GH1443/product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Hot caller | `crates/harness-server/src/workflow_runtime_worker/executor.rs:113-155` | Per runtime job: thread_manager start_thread/start_turn → `run_turn_lifecycle_with_options` (line 129) | Defines what must keep working |
| Move target 1 | `crates/harness-server/src/task_executor/turn_lifecycle.rs` | Turn execution loop: stream select, stall/wall-clock timeouts, failure strings | Live engine, wrongly homed |
| Move target 2 | `crates/harness-server/src/task_executor/helpers.rs` | Stream-item processing incl. `StreamItem::TokenUsage` at :259 | Live; also the GH-1439 fix site |
| Legacy siblings | `task_executor/{run_task,review_loop,agent_review,streaming}.rs` | Legacy POST /tasks pipeline (62 tasks ever); interceptors run only here | Stays behind; deleted by #1434 |
| Split target | `crates/harness-server/src/post_validator.rs` | `validate_command_safety` live via `workspace_helpers.rs:139` + `task_executor/validation_gate.rs:29`; `PostExecutionValidator` interceptor legacy-only | Split-before-delete |
| Consumers | `crates/harness-server/src/http/builders/*`, CLI | Construct services and import types from task_executor | Import-path updates |

## Proposed Design

1. Create `crates/harness-server/src/workflow_runtime_worker/turn_engine/` and `git mv` `turn_lifecycle.rs` and `helpers.rs` into it; update `mod` wiring and importers. Where a private helper is shared with legacy files, copy it into the legacy side and mark the copy `// legacy duplicate — dies with #1434`.
2. Move `validate_command_safety` (+ its tests) into a new `command_safety.rs` owned by workspace setup (`workspace_helpers.rs` caller); leave `post_validator.rs` holding only the legacy interceptor.
3. Add module doc comments: turn_engine = owned by workflow runtime; `task_executor/` = legacy task path pending #1434.
4. Enforce the boundary in CI-greppable form: no `use crate::task_executor` inside `workflow_runtime_worker/` (checked in review; optionally a unit test asserting module paths).
5. No logic edits: diffs limited to paths, imports, visibility (`pub(crate)` adjustments), and doc comments.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 identical execution | pure `git mv` + import fixes | Full existing test suite unmodified; `git log --follow` shows renames |
| P2 tests unchanged | test imports only | Diff review: no assertion edits (test-weakening guard) |
| P3 clean boundary | module wiring | `rg "task_executor" .../workflow_runtime_worker/` empty |
| P4 command safety intact | `command_safety.rs` | Existing validate_command_safety tests pass relocated |
| P5 ownership docs | doc comments | Review checklist |

## Data Flow

Unchanged: agent stream → turn_engine loop → thread_manager updates + notifications + persistence. Only the module path of the code changes.

## Alternatives Considered

- Do the split inside the #1434 deletion PR — rejected: mixes mechanical moves with deletions, making both harder to review and revert.
- Extract into a new `harness-turn` crate — rejected: crate count is being reduced (#1434 merges small crates); a module under the worker suffices.
- Leave permanent re-export shims in `task_executor` — rejected: shims are aliases (house rule U-24) and would keep the doomed directory referenced.

## Risks

- Security: none.
- Compatibility: internal module paths only; JSON-RPC/HTTP surfaces unchanged.
- Performance: none (no logic change).
- Maintenance: temporary duplication of small shared helpers on the legacy side; bounded — that code is deleted by #1434.

## Test Plan

- [ ] Unit tests: full existing suite green with zero assertion changes.
- [ ] Integration tests: workflow_runtime_worker job execution tests pass; one smoke run of a local runtime job (serve + dispatch) behaves identically.
- [ ] Manual verification: `git log --follow` on both moved files; boundary grep empty.

## Rollback Plan

Revert the single refactor PR; no data, config, or schema involved.
