# Product Spec

## Linked Issue

GH-1603

Complexity: large. This changes a cross-definition workflow-policy contract and the
operator evidence required for already-persisted violations.

## User Problem

Harness can persist a workflow in a nonterminal state that implies more automated
work while creating no command or runtime job to perform that work. The instance
continues to look active, but the dispatcher and workers have nothing to claim and
the wait-state watchdog does not classify states such as `implementing` or
`replanning` as stopped. Operators must discover these rows manually, and intake or
downstream workflow progress can remain blocked indefinitely.

## Goals

- Make ownership of the next step explicit for every registered nonterminal state.
- Reject transitions into command-driven states unless the same accepted decision
  durably creates work that can advance the workflow.
- Preserve intentional external waits, operator gates, and child-to-parent handoffs
  without treating them as defects.
- Make preexisting driverless progress states visible for explicit recovery without
  silently rewriting business state.

## Non-Goals

- Renaming states or replacing the current workflow definitions.
- Retrying commands that fail during dispatch; that failure policy is separate.
- Changing RemoteHost lease or completion behavior.
- Treating a timeout watchdog as the primary correctness mechanism.
- Automatically mutating or deleting preexisting driverless workflow rows.
- Redesigning child-to-parent propagation durability in this change.

## User-Visible Behavior

New workflow decisions cannot leave an instance in an automated progress state with
no durable owner. Invalid decisions are rejected at the transition boundary and
produce an auditable reason. States intentionally waiting on an external observer,
an operator, or parent propagation remain valid because that ownership is declared
explicitly. Operator diagnostics identify any preexisting command-driven instances
that have no active command or runtime job so they can be recovered deliberately.

## Behavior Invariants

1. **B-001** Every registered nonterminal workflow state declares exactly one
   progress mode from the closed set `command_driven`, `external_wait`,
   `operator_gate`, or `parent_handoff`; terminal states remain explicitly terminal.
2. **B-002** A decision that transitions into a `command_driven` state is accepted
   only when that same decision contains at least one eligible durable driver that
   can advance the target state.
3. **B-003** An omitted `commands` field and an explicit `commands: []` are
   equivalent and both fail B-002.
4. **B-004** A wait marker, audit-only command, inline bookkeeping command, or other
   command that cannot create claimable work does not satisfy B-002.
5. **B-005** A command-free transition into `external_wait`, `operator_gate`, or
   `parent_handoff` is accepted only when the target state's declared owner has a
   deterministic wake or resolution path; a descriptive reason alone is not a wake
   path.
6. **B-006** An authorized retry, unblock, recovery, or terminal reopen that targets
   a `command_driven` state obeys the same durable-driver requirement; authorization
   does not bypass liveness validation.
7. **B-007** A rejected driverless decision does not apply its requested state,
   version, or follow-up commands and records an auditable rejection reason. A
   separate policy decision may move the workflow to an explicitly stopped state,
   but it must be distinguishable from the rejected decision.
8. **B-008** Concurrent completion, retry, and replay cannot use a stale, terminal,
   unrelated, or merely dedupe-colliding command/job as a substitute for the driver
   required in the decision being committed.
9. **B-009** Preexisting command-driven instances are healthy only when the accepted
   decision that established the current state has an eligible active command or
   unfinished runtime job linked to that decision. Missing or ambiguous state-entry
   provenance, and work linked only to an older or unrelated decision, are reported
   with workflow id, definition, state, age, and provenance status for explicit
   recovery; rollout does not silently change state or erase history.
10. **B-010** Every transition target in every registered definition has progress
    metadata, and adding a state or allowlisted transition without that metadata
    fails a deterministic completeness check.
11. **B-011** The authoritative state matrix below is verified semantically: every
    command-driven row names an eligible durable-driver family, every external wait
    names an enabled deterministic observer, every operator gate names its explicit
    resolution action, and every parent handoff names its propagation path. A
    structurally present mode with no executable wake path fails verification.

## Authoritative Progress Ownership Matrix

This matrix exhausts the nonterminal entries in the current four-definition state
registry. It is the behavioral authority for `ProgressMode`; implementation must not
infer a different mode from an individual transition's optional command allowlist.

| Definition | Registered state(s) | `ProgressMode` | Concrete owner and deterministic wake/resolution path |
| --- | --- | --- | --- |
| `github_issue_pr` | `discovered` | `command_driven` | Submission bootstrap owns this initial-only state and must atomically commit the accepted submission decision that selects `planning`, `implementing`, or `awaiting_dependencies`; `discovered` is not a valid driverless steady state. |
| `github_issue_pr` | `scheduled`, `planning` | `command_driven` | The state-entry decision owns an `EnqueueActivity` driver for issue planning or implementation; runtime command dispatch creates the claimable job. |
| `github_issue_pr` | `implementing` | `command_driven` | The state-entry decision owns an `EnqueueActivity` driver for `implement_issue` (including a candidate command when fanout is used). |
| `github_issue_pr` | `replanning` | `command_driven` | The state-entry decision owns an `EnqueueActivity` driver for `replan_issue`. |
| `github_issue_pr` | `local_review_gate` | `command_driven` | The state-entry decision owns an `EnqueueActivity` driver for `run_local_review`. |
| `github_issue_pr` | `addressing_feedback` | `command_driven` | The state-entry decision owns `EnqueueActivity(address_pr_feedback)` or the declared PR-feedback child workflow. |
| `github_issue_pr` | `quality_gate_pending` | `parent_handoff` | A quality-gate child linked to this parent owns resolution; child completion is propagated to the parent and produces `ready_to_merge` or an explicit stopped outcome. |
| `github_issue_pr` | `merging` | `command_driven` | The authorized state-entry decision owns `EnqueueActivity(merge_pr)`. |
| `github_issue_pr` | `awaiting_dependencies` | `external_wait` | The runtime dependency-release observer re-evaluates persisted dependency identities and commits a release/failure decision. |
| `github_issue_pr` | `pr_open` | `external_wait` | The PR-feedback sweeper observes the bound PR and requests `run_local_review`. |
| `github_issue_pr` | `awaiting_feedback` | `external_wait` | The PR-feedback sweeper observes remote PR facts and requests a feedback sweep or PR-feedback child. |
| `github_issue_pr` | `ready_to_merge` | `operator_gate` | The explicit merge-approval action resolves the gate; configured auto-merge policy may invoke the same approval path but is not required for validity. |
| `github_issue_pr` | `blocked` | `operator_gate` | An authorized retry, unblock, recovery, cancellation, or failure-resolution action resolves the gate. |
| `prompt_task` | `submitted` | `command_driven` | Submission bootstrap atomically selects `implementing` or `awaiting_dependencies`; `submitted` is not a valid driverless steady state. |
| `prompt_task` | `implementing` | `command_driven` | The state-entry decision owns `EnqueueActivity(implement_prompt)`. |
| `prompt_task` | `awaiting_dependencies` | `external_wait` | The runtime dependency-release observer re-evaluates persisted dependency identities and commits release/failure. |
| `prompt_task` | `blocked` | `operator_gate` | An authorized retry, unblock, recovery, cancellation, or failure-resolution action resolves the gate. |
| `quality_gate` | `pending`, `checking` | `command_driven` | Child bootstrap/replay and the accepted run decision own `EnqueueActivity(run_quality_gate)`; `pending` may not remain after bootstrap without that decision-linked driver. |
| `quality_gate` | `blocked` | `operator_gate` | An authorized retry, recovery, cancellation, or failure-resolution action resolves the gate and the child outcome propagates to its parent when present. |
| `pr_feedback` | `pending`, `inspecting` | `command_driven` | Child bootstrap/replay and the accepted inspect decision own `EnqueueActivity(inspect_pr_feedback)`; `pending` may not remain after bootstrap without that decision-linked driver. |
| `pr_feedback` | `feedback_found`, `no_actionable_feedback`, `ready_to_merge` | `parent_handoff` | The runtime worker propagates the completed child result through the recorded `parent_workflow_id`; the names describe child outcomes, not parent gates. |
| `pr_feedback` | `blocked` | `operator_gate` | An authorized retry, recovery, cancellation, or failure-resolution action resolves the child gate and any terminal/stopped result is propagated explicitly. |

Terminal registry entries carry terminal metadata rather than a `ProgressMode`:
`github_issue_pr` and `prompt_task` use `done`, `failed`, and `cancelled`;
`quality_gate` uses `passed`, `failed`, and `cancelled`; `pr_feedback` uses `done`,
`failed`, and `cancelled`.

## Boundary Checklist

| Boundary | Coverage |
| --- | --- |
| Empty / missing input | B-002, B-003, and B-004 cover omitted, empty, and non-driving command lists. |
| Error and failure paths | B-007 defines fail-closed persistence and auditable rejection. |
| Authorization / permission | B-006 prevents recovery authority from bypassing liveness. |
| Concurrency / race / ordering | B-008 requires the driver to belong to the decision being committed. |
| Retry / repetition / idempotency | B-006 and B-008 cover replay, retry, and duplicate work. |
| Illegal state transitions | B-001, B-002, and B-010 make missing progress ownership invalid. |
| Compatibility / migration | B-009 preserves and reports historical rows without silent repair. |
| Degradation / fallback | B-005 and B-007 prevent a reason or fallback marker from looking like progress. |
| Evidence and audit integrity | B-007, B-009, and B-011 require durable rejection, provenance-scoped diagnostics, and semantically valid ownership. |
| Cancellation / interruption / partial completion | Terminal cancellation is unchanged; B-006 applies if an authorized reopen targets command-driven work, and B-005 covers partial parent handoff. |

## Acceptance Criteria

- [ ] Empty and omitted command lists are rejected for all command-driven target
      states across all registered workflow definitions.
- [ ] Wait-only and inline-only decisions cannot satisfy a command-driven target.
- [ ] Positive tests preserve valid external waits, operator gates, parent handoffs,
      and terminal transitions.
- [ ] Persistence tests prove that the rejected requested transition does not update
      the instance or enqueue its commands and that its reason remains auditable.
- [ ] Recovery tests prove authorized recovery into command-driven work includes an
      eligible durable driver.
- [ ] A deterministic completeness test covers every state and allowlist target.
- [ ] A table-driven semantic test matches every nonterminal registry row to the
      authoritative mode, owner, and executable wake/resolution path in this spec.
- [ ] Operator diagnostics report preexisting driverless progress instances without
      mutating them, and negative fixtures prove stale or unrelated commands/jobs do
      not hide a driverless current state.
- [ ] Focused workflow and server tests plus the cross-workspace compile check pass.

## Edge Cases

- A decision contains both audit commands and one eligible driver: the driver
  satisfies progress; existing command payload, budget, and dedupe validation still
  applies to every command.
- A target state is externally driven but also receives a command: the transition is
  valid only if that command is otherwise allowed; progress metadata does not widen
  the command allowlist.
- A prior command for the workflow is still active when a completion proposes the
  next state: it cannot substitute for the new decision's driver. A reusable durable
  ownership path must be explicitly re-associated with the accepted state-entry
  decision; workflow identity, command type, or a matching dedupe key alone is not
  provenance.
- The latest accepted decision cannot be shown to establish the persisted current
  state: the diagnostic reports missing/ambiguous state-entry provenance rather than
  treating any workflow command as proof of health.
- A child outcome is persisted before parent propagation: it is classified as
  `parent_handoff`, not `command_driven`; propagation atomicity remains separately
  governed.
- A stopped workflow is explicitly reopened: the destination's progress contract is
  applied after all existing authorization and recovery checks.
- An unknown workflow definition or state has no progress contract and fails closed.

## Rollout Notes

The state progress modes are code-owned metadata and require no database schema
migration. Enforcement applies to newly evaluated decisions. Before or alongside
enforcement, run the read-only diagnostic against existing rows and publish the
count and identifiers in implementation evidence. Any repair uses existing explicit
operator recovery paths; there is no automatic state rewrite. No public API field is
required unless the operator diagnostic is exposed through an existing monitoring
response.
