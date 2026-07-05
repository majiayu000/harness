# Tech Spec

## Linked Issue

GH-1469

## Product Spec

See `specs/GH1469/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Config | `crates/harness-core/src/config/misc.rs` | `ConcurrencyConfig::max_turns` is `Option<u32>` and defaults to `None`. | This makes the existing budget inert by default. |
| Request override | `crates/harness-server/src/task_runner/request.rs` | `TaskRequest::max_turns` can set a per-task budget. | Request-level overrides must keep priority. |
| Task executor | `crates/harness-server/src/task_executor/run_task.rs` | Effective budget is `req.max_turns.or(server_config.concurrency.max_turns)` and is threaded through implementation, review, gates, and review loop. | This is the existing cross-layer ceiling. |
| Transient retries | `crates/harness-server/src/task_runner/spawn.rs` | `total_turns_used` is carried across transient retry attempts. | Retries should not reset the task budget. |
| Implementation phase | `crates/harness-server/src/task_executor/implement_pipeline.rs` | Checks budget before each implementation attempt and increments after each agent invocation. | Counts implementation and validation retry calls. |
| Local review | `crates/harness-server/src/task_executor/agent_review.rs` | Checks budget before review and fix agent calls. | Counts review loop calls. |
| Hosted review loop | `crates/harness-server/src/task_executor/review_loop/flow.rs` | Checks budget before hosted review-loop calls and marks budget exhaustion. | Counts another retry/review layer. |
| Docs | `config/default.toml.example`, `docs/usage-guide.md`, `README.md` | Concurrency docs do not show a max-turns default. | Operators need to see the new default and override semantics. |

## Proposed Design

1. Add a default helper for legacy task `max_turns`.
   - Default value: `Some(20)`.
   - Keep `TaskRequest::max_turns` as the first-priority override.
   - Do not change workflow-runtime profile `max_turns`.
2. Update docs and examples.
   - `config/default.toml.example` should show `max_turns = 20` in the
     `[concurrency]` block.
   - `docs/usage-guide.md` should describe the default and request override.
   - README should keep its existing concurrency example coherent with the
     default.
3. Add focused tests.
   - Config default test verifies `ConcurrencyConfig::default().max_turns ==
     Some(20)`.
   - Request override test verifies explicit request `max_turns` wins over the
     global default.
   - Existing review/implementation budget tests should be reused where
     possible; add only the smallest missing test to prove the budget crosses at
     least two layers.

## Data Flow

Task submission creates a `TaskRequest`, optional request-level `max_turns`
rides with the request, and `run_task` computes
`req.max_turns.or(config.concurrency.max_turns)`. The same mutable
`turns_used` and `turns_used_acc` references flow through implementation,
validation retries, local review, hosted review loop, gates, and transient
retry attempts. Exhaustion returns an explicit error or marks the task with a
budget-exhausted status, depending on the layer's existing behavior.

## Alternatives Considered

- Add a separate invocation-budget config. Rejected because the current
  `max_turns` plumbing already represents total agent invocations across the
  legacy task lifecycle; a second counter would create disagreement.
- Keep unlimited by default. Rejected because the issue is specifically about
  unbounded default spend.
- Use a very small default. Rejected because normal implementation plus review
  can need several agent calls; 20 matches the existing production
  recommendation in config comments.

## Risks

- Some long-running tasks may fail earlier than before. Mitigate by documenting
  the value and allowing request/global overrides.
- A default change can affect tests that expected `None`. Mitigate by updating
  only explicit default assertions and preserving request overrides.
- Config file size constraints exist in `misc.rs`; keep the implementation small
  and avoid unrelated refactors.

## Test Plan

- [ ] Run `cargo test -p harness-core config`.
- [ ] Run the focused task-executor budget tests added or updated by this work.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1469`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before PR
      merge readiness.

## Rollback Plan

Revert the implementation commit. The legacy task budget returns to unlimited
by default while preserving existing request-level `max_turns` behavior.
