# GH1609 Product Spec: Declarative Workflow Definitions from WORKFLOW.md

GitHub issue: `#1609`
Depends on: GH-1608 (owned definition registry)

## Goals

- Make a repo's `WORKFLOW.md` sufficient to define and run a new workflow
  shape — states, driving activities, transitions, terminal mapping,
  required evidence — without Rust changes, on top of the existing durable
  runtime (leases, retries, circuit breaker, audit).
- Validate declared shapes as strictly as code: structural errors fail at
  startup with actionable messages, never at workflow runtime.
- Pin every workflow instance to the definition content it was created
  under, so editing WORKFLOW.md mid-flight never mutates a running
  instance's shape.
- Keep the four built-in definitions and their reducers completely
  untouched.

## Non-Goals

- No migration of built-in definitions (`github_issue_pr`, `pr_feedback`,
  `quality_gate`, `prompt_task`) to declarations.
- No child workflows, candidate fanout, or plan-issue mechanics in
  declarative definitions v1.
- No hot-reload of definition changes into in-flight instances.
- No new activity execution kinds; declared activities dispatch through
  the existing runtime profiles and activity policy map.

## Users

- Repo owners who want a custom flow (for example docs-review, triage, or
  release checklists) driven by their WORKFLOW.md without forking harness.
- The Symphony-parity operating mode: WORKFLOW.md becomes the single file
  that defines both behavior (prompts) and shape (states/transitions).

## Behavior Invariants

- `B-001` A WORKFLOW.md without a `definition` block changes nothing:
  no new registration, byte-identical config parsing for existing files.
- `B-002` An invalid `definition` block fails config load/startup with an
  actionable error naming the state or transition at fault; there is zero
  partial registration (a definition registers whole or not at all).
- `B-003` Structural validation rejects: transitions to undeclared
  states, states unreachable from the declared initial state, missing or
  incomplete terminal mapping (done/failed/cancelled classes), duplicate
  state names, ids colliding with built-in or already-registered
  definitions, and non-terminal states lacking a progress mode (a driving
  `activity`, `progress: external_wait`, or `progress: operator_gate`) —
  aligned with GH-1603.
- `B-004` Valid declarations register at server startup through the
  GH-1608 registry, before the registry freezes and before any dispatch
  begins.
- `B-005` `definition_version` on each instance is a content hash of the
  canonicalized declaration. In-flight instances complete under their
  pinned version: a changed WORKFLOW.md affects only instances created
  after reload, and version mismatch between an instance and the loaded
  declaration is detected, surfaced, and blocks interpretation for that
  instance rather than silently applying the new shape.
- `B-006` The generic interpreting reducer is reachable only for
  registered declarative definition ids; built-in ids never enter the
  interpreter and their reducers/validators are byte-identical in
  behavior.
- `B-007` Every interpreted transition into a non-terminal
  activity-driven state carries its driving `enqueue_activity` command in
  the same completion transaction; terminal transitions carry their
  Mark* command — no driverless progress states (GH-1603).
- `B-008` Signal mapping: `on_signal` entries match by signal type;
  an unmapped signal on an otherwise successful result follows
  `on_success`; a declared signal mapping always wins over `on_success`.
  A result whose status is Blocked/Failed follows `on_blocked`/
  `on_failure` when declared, and the existing generic blocked/retry
  fallbacks when not.
- `B-009` `evidence_required` is enforced for declared transitions: a
  decision missing a required evidence kind is rejected with an auditable
  reason, mirroring built-in terminal-evidence rules.
- `B-010` Declared activity names bind to the existing `activities`
  policy map (prompt selection, validation commands) and dispatch through
  existing runtime profile selection; an activity declared in the
  definition but absent from the activity policy map fails startup
  validation (B-002 class), not runtime.
- `B-011` An activity completion arriving for a (state, activity) pair
  the declaration does not expect follows the existing
  invalid-agent-output blocked fallback — never an implicit transition.
- `B-012` The `definition` block does not deep-merge: a repo WORKFLOW.md
  `definition` replaces the central base's declaration for that id
  wholesale. Field-by-field merging of state machines is rejected by
  design; the loader treats `definition` as an atomic value.
- `B-013` Operator recovery transitions for declarative workflows
  (blocked → an active state) are permitted only to states declared as
  recovery targets and only through the existing operator action context,
  mirroring built-in recovery gating.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-001 (no block = no change), B-003 (empty/incomplete declarations rejected) |
| Failure paths | covered: B-002 (startup failure, zero partial registration), B-008 (failure/blocked routing), B-011 (unexpected completion → blocked fallback) |
| Authorization | covered: B-013 (operator-gated recovery); definition source is the repo's own WORKFLOW.md — same trust surface as existing prompt/policy config |
| Concurrency | covered: B-004 (registration before freeze/dispatch), B-007 (same-transaction drivers) |
| Retry + idempotency | covered: B-010 (existing dispatch/retry machinery unchanged), B-005 (version pinning stable across restarts) |
| Illegal transitions | covered: B-003 (structural validation), B-009 (evidence gates), B-011 (no implicit transitions) |
| Compatibility | covered: B-001, B-006 (built-ins untouched), B-012 (atomic block, no surprise merges) |
| Degradation / fallback | covered: B-005 (version mismatch surfaces, never silently reshapes), B-011 (blocked, not guessed) |
| Evidence / audit | covered: B-009 (required evidence), B-007 (decisions carry commands and reasons like built-ins) |
| Cancellation / partial | covered: terminal mapping must include cancelled (B-003); in-flight pinning under definition edits (B-005) |

Cross-product boundary called out explicitly: a WORKFLOW.md edit while an
instance is mid-flight must leave the instance on its pinned shape and
the operator able to see both versions (B-005 + B-013), and a declaration
that validates structurally but references an activity with no policy
entry must die at startup, not strand a workflow at dispatch time
(B-003 + B-010).
