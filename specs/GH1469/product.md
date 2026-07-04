# Product Spec

## Linked Issue

GH-1469

## User Problem

A persistently failing task can consume tens of agent CLI invocations (real
API spend) because `concurrency.max_turns` defaults to `None` and the retry
layers (in-spawn transient retry, agent review rounds, hosted review-bot
rounds, periodic re-enqueue, workflow activity retries) multiply with no
shared ceiling. The operator only notices via the bill.

## Goals

- Ship a safe non-None default for `max_turns`.
- Count every agent spawn attributable to a task across all retry layers.
- Stop spending when a configurable per-task total budget is exhausted, with
  an explicit terminal failure reason.

## Non-Goals

- Changing quota/billing circuit-breaker semantics.
- Per-project differentiated budgets (later).

## Behavior Invariants

1. A task never triggers more than `invocation_budget` agent spawns in total,
   across all retry layers, for its lifetime.
2. Budget exhaustion produces a terminal task state with an explicit,
   user-visible reason — never a silent stall or endless retry.
3. Existing configs that set `max_turns` explicitly keep their value; only the
   unset case changes behavior.
