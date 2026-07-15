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
9. **B-009** Preexisting command-driven instances with no active command or runtime
   job are reported with workflow id, definition, state, and age for explicit
   recovery; rollout does not silently change their state or erase their history.
10. **B-010** Every transition target in every registered definition has progress
    metadata, and adding a state or allowlisted transition without that metadata
    fails a deterministic completeness check.

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
| Evidence and audit integrity | B-007 and B-009 require durable rejection and diagnostic evidence. |
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
- [ ] Operator diagnostics report preexisting driverless progress instances without
      mutating them.
- [ ] Focused workflow and server tests plus the cross-workspace compile check pass.

## Edge Cases

- A decision contains both audit commands and one eligible driver: the driver
  satisfies progress; existing command payload, budget, and dedupe validation still
  applies to every command.
- A target state is externally driven but also receives a command: the transition is
  valid only if that command is otherwise allowed; progress metadata does not widen
  the command allowlist.
- A prior command for the workflow is still active when a completion proposes the
  next state: it cannot substitute for the new decision's driver unless the
  transition contract explicitly identifies it as the same durable ownership path.
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
