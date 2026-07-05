# Product Spec

## Linked Issue

GH-1469

## User Problem

Harness already has a per-task `max_turns` budget that is threaded through the
task executor, validation retry path, agent review path, hosted review loop, and
transient task retry accumulator. However, the global
`concurrency.max_turns` default is currently `None`, so operators who do not
set it explicitly get an unbounded task-level agent invocation budget.

That means a persistently failing task can consume many agent CLI invocations
across retries and review loops before a human sees the failure.

## Goals

- Make the default per-task agent invocation budget non-empty.
- Use the existing `max_turns` budget path as the cross-layer ceiling rather
  than adding a second counter.
- Preserve request-level `max_turns` overrides as the highest-precedence budget.
- Surface budget exhaustion as an explicit task failure/error reason.
- Document the default budget and where operators can raise it.

## Non-Goals

- Changing quota/billing circuit breaker behavior.
- Adding per-project differentiated budgets.
- Changing workflow-runtime profile `max_turns`; that is a separate runtime
  budget path.
- Rewriting review-loop or transient retry behavior beyond honoring the shared
  budget.

## User-Visible Behavior

With default configuration, legacy task execution should stop after 20 total
agent invocations for one task unless a request supplies a smaller or larger
`max_turns` override. The count includes implementation attempts, validation
auto-retries, local agent review rounds, hosted review-loop rounds, provider
review gates, non-implementation tasks, and transient task retries that re-run
the task.

When the budget is exhausted, the task should fail or stop retrying with an
explicit message that names the turn budget exhaustion instead of silently
continuing.

## Acceptance Criteria

- [ ] `concurrency.max_turns` defaults to `Some(20)`.
- [ ] Config examples and usage docs document the default and that request-level
      `max_turns` overrides the global value.
- [ ] Existing explicit request-level `max_turns` overrides continue to win over
      the global default.
- [ ] Tests cover budget counting across at least two execution layers.
- [ ] Budget exhaustion remains explicit in task failure/error output.

## Edge Cases

- A request with an explicit low `max_turns` should still be able to fail fast
  before the default is reached.
- Transient task retries should continue from the previous attempt's turn count
  instead of resetting the budget.
- Operators with unusually long local workflows can raise the global default;
  the implementation should not add a hard-coded ceiling outside config.

## Rollout Notes

Use `Refs #1469` for the spec PR. The implementation PR should use
`Closes #1469` after the default, docs, and tests land.
