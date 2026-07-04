# Zero-Output Completion Detection — Design Spec

Status: Draft v0 (proposal, pending review)
Refs: #1477
Audience: harness-server reviewers + future implementer

## 1. Problem

Agent lanes can complete 0.5–2s after receiving the prompt with no assistant message
and no tool call, yet still emit `task_complete`. Any queue driver reading
`task_complete` counts the work as done, so the failure is invisible to the workflow
layer and to operators.

The failing sessions share a fixed event signature: exactly 9 events —
`session_meta`, `turn_context`, 3× instruction messages, `user_message`,
`task_started`, `token_count`, `task_complete` — with sub-second latency between
`user_message` and `task_complete` (e.g. 05:26:22.371Z → 05:26:22.857Z).

## 2. Evidence

Audit of 132 Codex implx sessions (2026-06-30 → 2026-07-04) driven by the harness
runtime and SpecRail queue workflows. On the evening of 2026-07-03, 4 of 22 lanes
(18%) died this way; two were real implementation tasks (ccstats GH38, GH47-T6
workers) whose work silently never started, and a PR #485 reviewer lane had to be
re-spawned manually later.

Local evidence files (audit machine):

- `~/.codex/sessions/2026/07/03/rollout-2026-07-03T13-26-17-*.jsonl`
- `~/.codex/sessions/2026/07/03/rollout-2026-07-03T13-26-34-*.jsonl`
- `~/.codex/sessions/2026/07/03/rollout-2026-07-03T16-52-22-*.jsonl`
- `~/.codex/sessions/2026/07/03/rollout-2026-07-03T22-04-26-*.jsonl`
- `~/.codex/sessions/2026/07/04/rollout-2026-07-04T03-53-32-*.jsonl`

## 3. Goals

- A terminal `done`/`succeeded` for an agent-executed activity requires observed
  agent activity: at least one assistant message, at least one tool invocation, or a
  structured result artifact.
- Zero-output completions are classified as spawn failures (a distinct failure
  class), not as success and not as generic task failure.
- Spawn failures are retried under the runtime retry policy and escalated when
  retries are exhausted, with the classification visible in task events and logs.

## 4. Non-Goals

- Not implementing stall/inactivity detection for long-running no-progress turns —
  that is #1437 (the opposite failure mode).
- Not implementing the respawn backoff / circuit-breaker policy — that is #1478,
  which consumes the classification introduced here.
- Not changing the agent adapters' CLI argument construction or the event wire
  format emitted by agents.
- Not validating the semantic quality of agent output; only its existence.

## 5. Proposed Behavior

The turn-completion path (the layer that converts a finished agent turn into an
activity/task outcome, cf. `crates/harness-server/src/workflow_runtime_worker/activity_result.rs`
and the task runner completion path) gains an activity gate:

1. While a turn runs, the runtime counts observed agent activity signals:
   assistant messages, tool invocations, and structured result artifacts.
2. When the agent process reports completion, the gate checks the counters.
3. If all counters are zero, the outcome is `SpawnFailure` instead of success.
   The event is logged at error level with the session id, duration, and event
   count.
4. `SpawnFailure` feeds the existing retry machinery; retry exhaustion escalates
   to a failed (terminal, visible) state — never a silent success.

## 6. Behavior Invariants

1. A turn that produced zero assistant messages, zero tool invocations, and no
   structured result artifact MUST NOT yield a `succeeded` activity result,
   regardless of the agent process exit status or a `task_complete` event.
2. A zero-output completion MUST be classified as a spawn failure, distinct from
   task-level failure, and the classification MUST appear in the task event stream.
3. Zero-output classification MUST be logged at error level (not warning), naming
   the task id, workspace, and turn duration.
4. A turn with at least one assistant message OR at least one tool invocation OR a
   structured result artifact MUST NOT be classified as a spawn failure by this
   gate (it may still fail for other reasons).
5. Spawn failures MUST be retried under the configured retry policy; when retries
   are exhausted the task MUST reach a terminal failed state visible to queue
   drivers, never a non-terminal or succeeded state.
6. The gate MUST be applied uniformly to all agent kinds the runtime spawns; it
   MUST NOT be special-cased to one adapter.

## 7. Acceptance Criteria

- Unit tests: a synthetic turn with only lifecycle events (`task_started`,
  `token_count`, `task_complete`) produces a spawn-failure result; a turn with one
  assistant message produces a non-spawn-failure result; a turn with only a tool
  call produces a non-spawn-failure result.
- Integration test: a queue driver observing the task sees `failed` (after retry
  exhaustion) rather than `done` for a persistently zero-output agent.
- Log assertion: the error-level classification line is emitted exactly once per
  zero-output turn.
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` and
  `cargo test` pass.

## 8. Rollout Notes

- Ship behind a config flag defaulting to ON (e.g.
  `runtime.require_agent_activity = true`) so an operator hitting a false positive
  can disable it while reporting the case.
- Metrics: add a spawn-failure counter to the usage/health surfaces so the
  circuit-breaker work in #1478 has a ready signal.
- No schema or wire-format changes expected; classification rides existing task
  event and result types.
