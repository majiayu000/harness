# GH1607 Tech Spec: prompt_task Continuation Loop

Product spec: `specs/GH1607/product.md`
GitHub issue: `#1607`
Related: GH-1609 (declarative definitions reuse the signal-mapping shape), GH-1603 (no driverless progress states)

## Codebase Context (verified anchors)

- prompt_task definition: `crates/harness-workflow/src/runtime/prompt_task.rs:3-4`
  (`PROMPT_TASK_DEFINITION_ID`, `PROMPT_TASK_IMPLEMENT_ACTIVITY`), submission
  decision builder `prompt_task.rs:28` (`build_prompt_submission_decision`,
  states `submitted`/`awaiting_dependencies` → `implementing`).
- States: `crates/harness-workflow/src/runtime/state_registry.rs:92-112`
  (`PROMPT_TASK_STATES`: submitted, awaiting_dependencies, implementing,
  blocked, done/failed/cancelled).
- Success reduction: `crates/harness-workflow/src/runtime/reducer.rs:307-311`
  (`(prompt_task, implementing, implement_prompt) → done`), MarkDone command
  attachment `reducer.rs:327-340`.
- Structured output contract:
  `crates/harness-workflow/src/runtime/model.rs:679-683` (`ActivitySignal`),
  `model.rs:718-732` (`ActivityResult.signals`), `model.rs:76-91`
  (`WorkflowInstance.data` for persisted loop state).
- Validator contract:
  `crates/harness-workflow/src/runtime/validator.rs:13-50`
  (`TransitionRule`) matches only the from/to states and allowed command
  types; it has no decision-name predicate. `prompt_task_defaults()` at
  `validator.rs:297-323` already permits `implementing → implementing`
  with `EnqueueActivity` or `Wait` at line 316, so that rule is broader
  than this feature's intended continuation decision. Validator selection
  remains at `reducer.rs:459`; workflow-specific validation currently
  distinguishes only `GithubIssuePr` from `Generic` at
  `validator.rs:425-456,613-621`.
- Server submission path:
  `crates/harness-server/src/workflow_runtime_submission.rs:198` (calls
  `build_prompt_submission_decision`).
- Command enqueue helper: `model.rs:248-254`
  (`WorkflowCommand::enqueue_activity` with dedupe key).
- Blocked fallback for contract violations:
  `runtime/reducer/support.rs` (`invalid_agent_output_blocked_decision`,
  used at `reducer.rs:318`).

## Proposed Design

### Continuation policy (harness-workflow model)

```rust
// runtime/prompt_task.rs
pub struct PromptContinuationPolicy {
    pub max_attempts: u32,        // >= 1, server cap 20 (B-002)
    pub attempt_delay_secs: u64,  // cap 3600 (B-002)
    pub active_states: BTreeSet<String>, // non-empty; set semantics: no duplicates, O(log n) lookup (B-002)
    pub no_progress_limit: u32,   // default 3 (B-008)
}
```

- Carried on `PromptSubmissionDecisionInput` as `Option<&PromptContinuationPolicy>`;
  validated by a `validate()` with explicit errors (B-002).
- Persisted under `instance.data["continuation"]` at submission:
  `{ policy, attempt: 1, last_external_state: null, last_summary: null,
  same_state_count: 0 }` (B-009). `last_external_state` and
  `last_summary` are updated by every continue decision so the prompt
  packet builder reads attempt context from instance data alone, without
  querying past job outputs (B-011). Absent key = single-shot semantics
  (B-001).

### Signal contract

The implement activity's `ActivityResult` must carry:

```json
{ "signal_type": "external_state",
  "signal": { "state": "In Progress", "subject": "TEAM-123" } }
```

Payload validation is strict: the `signal` value must be a JSON object
(`is_object()` checked before any field access), with a string `state`
field. `signal.state` is compared (case-sensitive, exact) against
`policy.active_states`. Zero or multiple `external_state` signals, a
non-object payload, or a missing/non-string `state` field is a contract
violation (B-006).

### Reducer changes (`reducer.rs`)

Replace the fixed `(prompt_task, implementing, implement_prompt) → done`
arm with a function `prompt_task_success_decision(instance, event, result)`:

1. No `continuation` key in `instance.data` → current behavior: `done` +
   MarkDone (B-001, B-005 path shared).
2. Policy present → parse the `external_state` signal:
   - malformed/missing/ambiguous → `invalid_agent_output_blocked_decision`
     variant `prompt_continuation_signal_missing` (B-006);
   - state ∉ `active_states` → `done` + MarkDone, decision
     `finish_prompt_task_external_settled` (B-005);
   - state ∈ `active_states`:
     - `attempt >= max_attempts` → `blocked`, decision
       `prompt_continuation_exhausted`, reason carries last state (B-007);
     - no-progress attempt (same reported state as `last_external_state`
       AND `result.artifacts` empty AND `result.validation` empty — the
       B-008 definition) for `no_progress_limit` consecutive attempts →
       `blocked`, decision `prompt_continuation_no_progress`; any
       progress attempt resets `same_state_count` (B-008);
     - otherwise → next_state `implementing`, decision
       `continue_prompt_task`, command
       `enqueue_activity(implement_prompt, "prompt-task:{id}:attempt:{n+1}")`
       plus a data patch updating `attempt`, `last_external_state`,
       `last_summary` from `result.summary`, and `same_state_count`
       (B-004, B-009, B-011). The patch and enqueue command commit in the
       same completion transaction, so the next prompt packet can read its
       complete continuation context from persisted instance data.
       Attempt-scoped dedupe keys make restart-resume idempotent.
3. Every branch attaches the parsed signal as `WorkflowEvidence`
   (`kind: "external_state"`) on the decision (B-013).

Instance-data patching uses the same mechanism as existing reducers that
mutate `instance.data` through decision commands; if no such mechanism
exists for data patches inside a decision, add a
`WorkflowCommandType::PatchInstanceData`-free approach: the completion
transaction already persists the instance, so the store applies the
`continuation` counters and `last_summary` when committing a
`continue_prompt_task` decision (implementation detail to pin in T002;
the invariant is atomic persistence with the transition and enqueue,
B-004/B-009/B-011).

### Validator changes (`validator.rs`)

The state-only `TransitionRule` cannot express the required decision-level
guard. Extend the existing workflow-specific validation path instead:

1. Add `PromptTask` to `DecisionValidatorKind`; make
   `DecisionValidator::prompt_task()` construct that kind rather than the
   current generic validator.
2. Route `PromptTask` through a dedicated
   `validator_prompt_task.rs::validate_decision`, following the existing
   `validator_github_issue_pr.rs` module boundary.
3. For every `implementing → implementing` prompt-task decision, validate
   fail closed: the decision ID must be exactly `continue_prompt_task`, and
   it must contain exactly one `EnqueueActivity` command whose payload
   names `implement_prompt`. Any other decision ID, a missing command, a
   `Wait`-only command list, an enqueue for another activity, or multiple
   enqueue commands is rejected before commit with a dedicated
   `InvalidDecisionContract` rejection. Future self-loop decisions remain
   rejected until explicitly added to this validator.
4. Narrow the existing allowlist entry at `validator.rs:316` from
   `[EnqueueActivity, Wait]` to `[EnqueueActivity]`; the workflow-specific
   rule supplies the decision-name, exact-count, and activity-payload
   constraints that the allowlist cannot represent.

The three continuation block decisions continue to use the existing
`allow_from_any("blocked", ...)` rule at `validator.rs:320`; the existing
`required_command_for_transition` mapping at `validator.rs:723-736`
requires `MarkBlocked`. No duplicate transition rule is added.

### Attempt context injection (harness-server)

The prompt packet builder for `implement_prompt` reads
`instance.data.continuation` and, when `attempt > 1`, prepends a
continuation header (attempt number, previous external state, previous
attempt summary from the last `ActivityResult`) to the agent prompt
(B-011). Anchor: prompt packet construction under
`crates/harness-server/src/workflow_runtime_worker/` (exact function to
pin during T005; it already renders per-activity prompt context).

### Delay between attempts

`attempt_delay_secs > 0` sets the enqueued command's earliest-dispatch
time using the existing retry-delay mechanism
(`runtime_retry_policy`-style `not_before`, see
`runtime/dispatcher.rs:408` `retry_not_before_for_command`). No new
scheduler.

## Edge Cases

- Agent reports `external_state` but also emits `SCOPE_TOO_LARGE` or
  another blocking signal: blocking signals win; the continuation check
  runs only on the success path after existing signal handling.
- Restart between completion commit and dispatch of attempt N+1: the
  outbox command with the attempt-scoped dedupe key survives; dispatch
  resumes; re-reduction of the same completion event is idempotent via
  the dedupe key (B-004, B-009).
- Operator cancels while attempt N is running: existing cancel path marks
  the instance; the completion reducer observes a terminal instance and
  produces no continuation (stale-completion guard, `reducer.rs:384-399`
  pattern) (B-012).
- If the tracker renames an active state and the reported value is absent
  from configured `active_states`, the reducer follows the settled branch
  and transitions to `done` (B-005). Operators must update the policy when
  tracker state names change; this case does not enter the no-progress
  guard.

## Migration / Compatibility

- `instance.data` is schemaless JSON; adding the `continuation` key needs
  no migration. Existing instances have no key → single-shot (B-001).
- Submission wire format: new optional `continuation` object; absent field
  deserializes to `None` (additive, B-001).
- No state-registry changes: the loop reuses the existing `implementing`
  state.

## Verification Plan

- Unit (`cargo test -p harness-workflow`): policy validation bounds
  (B-002); reducer branches — no policy, settled, continue, malformed
  signal, exhaustion, no-progress (B-001, B-004..B-008); validator
  self-transition contract. Named validator tests prove the positive
  `continue_prompt_task` case and reject an arbitrary decision ID, no
  command, `Wait` only, wrong activity, and multiple enqueue commands.
  The continue-branch test also reloads the committed instance and asserts
  that `last_summary == result.summary` before prompt construction.
- Persistence (`cargo test -p harness-workflow store`): counters and
  policy survive a store round-trip; attempt-scoped dedupe key idempotency
  (B-009).
- Server (`cargo test -p harness-server`): submission accepts/rejects
  policies (B-002); prompt packet carries attempt context (B-011);
  cancel-between-attempts enqueues nothing (B-012).
- Full gates: `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

## Rollback Plan

Feature is inert without a submitted continuation policy (B-001). Rollback
= stop submitting policies; in-flight looping instances finish via
exhaustion/no-progress bounds or operator cancel. Code revert removes the
reducer branch and validator rules; single-shot arm is restored verbatim.

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | reducer no-policy branch | `cargo test -p harness-workflow prompt_task` (single-shot unchanged) |
| B-002 | `PromptContinuationPolicy::validate` + submission | `cargo test -p harness-workflow prompt_task::policy` + `cargo test -p harness-server workflow_runtime_submission` |
| B-003 | design-level (no tracker client added) | review gate: no new HTTP/tracker dependency in diff |
| B-004 | continue branch + same-transaction command + prompt-task decision validator | `cargo test -p harness-workflow reducer::prompt_continuation` and `cargo test -p harness-workflow prompt_task_validator` (only `continue_prompt_task` with exactly one `implement_prompt` enqueue self-loops) |
| B-005 | settled branch reuses done path | `cargo test -p harness-workflow reducer::prompt_continuation` |
| B-006 | malformed-signal branch | `cargo test -p harness-workflow reducer::prompt_continuation` (blocked, not done) |
| B-007 | exhaustion branch | same module (attempt cap) |
| B-008 | no-progress counter | same module (K identical states → blocked) |
| B-009 | data persistence + dedupe keys | `cargo test -p harness-workflow store` round-trip |
| B-010 | no changes to job path (inherited) | existing worker/dispatch suites stay green |
| B-011 | prompt packet header | `cargo test -p harness-server prompt_packet` |
| B-012 | stale-completion guard reuse | `cargo test -p harness-workflow reducer::prompt_continuation` (terminal instance → no enqueue) |
| B-013 | evidence attachment on all branches | assertions in each reducer test |
