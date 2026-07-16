# WORKFLOW.md: Declarative Definitions and Continuation Policies

This page documents the two WORKFLOW.md features that let a repository
define workflow behavior without Rust changes:

- the **`definition` block** — declare a custom workflow shape (states,
  activities, transitions, terminal mapping, evidence) interpreted by the
  runtime (GH-1609);
- the **continuation policy** — run a single prompt task as a bounded
  loop that re-invokes the agent while an external tracker subject stays
  active (GH-1607).

Both features are inert unless configured. The examples below are
validated by `crates/harness-workflow/tests/docs_examples.rs`, which
parses this file and runs the real structural validation, so they cannot
drift from the implementation.

## Declarative workflow definitions

Add a `definition` block to the YAML frontmatter of a managed repo's
`WORKFLOW.md`. At server startup the declaration is structurally
validated, compiled, and registered; instances then run on the existing
durable runtime (leases, retries, circuit breaker, audit).

<!-- doc-example:declarative-definition -->
```yaml
activities:
  implement_issue:
    prompt: default
    validation:
      - cargo check
  run_local_review:
    prompt: pr_feedback

definition:
  id: docs_review_flow
  initial: implementing
  states:
    implementing:
      activity: implement_issue
      on_success: reviewing
      on_signal:
        SCOPE_TOO_LARGE: blocked
    reviewing:
      activity: run_local_review
      on_success: done
      on_failure: implementing
    blocked:
      progress: operator_gate
  terminal:
    done: succeeded
    failed: failed
    cancelled: cancelled
  evidence_required:
    done: [github_pr]
  recovery_targets:
    - implementing
```

### Schema rules (enforced at startup)

Startup fails with an actionable error — never a partially registered
definition — when a declaration violates any of these:

- `id` must not be empty and must not collide with a built-in definition
  (`github_issue_pr`, `prompt_task`, `quality_gate`, `pr_feedback`).
- `initial` must name a declared active state (not a terminal one).
- Every active state declares **exactly one** progress mode: an
  `activity` (which must exist in the `activities` policy map) **or**
  `progress: external_wait` / `progress: operator_gate`.
- A `blocked` active state is **required**, must declare exactly
  `progress: operator_gate`, no activity, and no outgoing routes. It is
  the runtime's safety fallback for invalid agent output; operators leave
  it through `recovery_targets`.
- `terminal` must assign **exactly one** state to each class:
  `succeeded`, `failed`, and `cancelled`.
- Every routing target (`on_success`, `on_failure`, `on_blocked`,
  `on_signal` values) must name a declared state; every declared state
  must be reachable from `initial` (`blocked` and the failed/cancelled
  terminals are implicitly reachable from any active state).
- `evidence_required` may not target `blocked` and may not repeat an
  evidence kind; `recovery_targets` must name active states other than
  `blocked`.

### Routing semantics

- On a successful activity completion, a declared `on_signal` match wins
  over `on_success`; when several mapped signal types appear on one
  result, the lexicographically smallest signal type wins and the
  decision records the tie.
- Blocked/failed activity results follow `on_blocked` / `on_failure` when
  declared, otherwise the runtime's generic blocked/retry handling.
- A completion for a `(state, activity)` pair the declaration does not
  expect lands in `blocked` — never an implicit transition.
- Transitions into terminal states emit the matching command:
  `succeeded` → `MarkDone`, `failed` → `MarkFailed`, `cancelled` →
  `MarkCancelled`.

### Pinning: editing WORKFLOW.md while instances are in flight

Each instance pins the definition content at creation:
`definition_version` (truncated content hash) plus the full hash in
instance data. Editing the `definition` block affects only instances
created after the next startup. An in-flight instance whose pinned
version is no longer registered transitions to `blocked` with
`definition_version_missing` — it is never silently reinterpreted under
the new shape. Operators resolve by restoring the old declaration or
cancelling the instance.

### Merge semantics

The `definition` block does **not** deep-merge. A repo-level `definition`
replaces the central base's declaration wholesale; state machines are
never merged field-by-field.

## Prompt-task continuation policies

A prompt task submission may carry an optional `continuation` object.
Without one, prompt tasks stay single-shot. With one, the runtime
re-enqueues the implement activity — with attempt context — while the
agent reports the external subject as still active.

<!-- doc-example:continuation-policy -->
```json
{
  "prompt": "Work the tracker ticket per the status map in this repo's WORKFLOW.md.",
  "continuation": {
    "max_attempts": 10,
    "attempt_delay_secs": 300,
    "active_states": ["Todo", "In Progress", "Rework"],
    "no_progress_limit": 3
  }
}
```

- `max_attempts` (≥ 1, server-capped) bounds the loop.
- `attempt_delay_secs` delays dispatch of the next attempt.
- `active_states` is the exact, case-sensitive set of external states
  that mean "keep going".
- `no_progress_limit` (default 3) bounds attempts that report the same
  external state with no new artifacts and no new validation records.

### The agent's signal contract

The harness process never contacts the tracker; the agent is the probe.
Each implement attempt must end with **exactly one** `external_state`
signal on its activity result, whose payload is a JSON object with a
non-empty string `state` field:

<!-- doc-example:external-state-signal -->
```json
{
  "signal_type": "external_state",
  "signal": {
    "state": "In Progress",
    "subject": "TEAM-123"
  }
}
```

The reported `state` is compared against `active_states`:

- state in `active_states`, attempts remaining → the workflow re-enters
  `implementing` and the next attempt is enqueued in the same
  transaction, with `attempt`, `previous_external_state`, and the
  previous attempt summary injected into the prompt packet as
  `continuation_context`.
- state not in `active_states` → the task completes as `done` through
  the normal evidence path.

### Blocked outcomes (never silent)

The loop terminates in `blocked` — with an auditable reason — when:

- the signal is missing, duplicated, a non-object payload, or lacks a
  string `state` (malformed contract);
- the attempt budget is exhausted while the subject is still active;
- `no_progress_limit` consecutive attempts report the same state with no
  new artifacts and no new validation records.

A continuation can never silently complete: malformed signals and
exhaustion both stop in `blocked`, and operator cancellation between
attempts ends the loop without enqueueing further work.
