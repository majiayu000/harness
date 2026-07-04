# Tech Spec

## Linked Issue

GH-1438

## Product Spec

See `specs/GH1438/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Runtime reconciliation | `crates/harness-server/src/reconciliation.rs` | `runtime_transition_for_github_state` maps merged PRs to target state `done`, and `apply_runtime_workflow_transition` builds a `MarkDone` decision for the current workflow state. | This is where the rejected `local_review_gate -> done` proposal is created. |
| Transition validation | `crates/harness-workflow/src/runtime/validator.rs` and `validator_github_issue_pr.rs` | The default allowlist permits `done` from several states, but not from `local_review_gate`. A special validator currently allows `blocked -> done` only for PR-merge reconciliation. | GH1438 needs the same evidence-bound exception for `local_review_gate`, without broadly weakening the gate. |
| Reconciliation tests | `crates/harness-server/src/reconciliation_tests.rs`, `reconciliation_blocked_done_tests.rs`, `reconciliation_ready_to_merge_tests.rs` | Existing tests cover merged PR reconciliation from other states. | Add regression coverage for `local_review_gate`. |
| Validator tests | `crates/harness-workflow/src/runtime/validator_tests.rs` | Existing tests prove the `blocked -> done` exception is reconciliation-only and evidence-bound. | Add equivalent rejection and allow tests for `local_review_gate`. |

## Proposed Design

1. Extend the GitHub issue PR validator with a reconciliation-only
   `local_review_gate -> done` exception when:
   - the actor is `reconciliation`;
   - the decision name is `reconcile_pr_merged`;
   - the decision contains a `MarkDone` command;
   - the command payload includes merged PR evidence compatible with the
     existing reconciliation evidence rules.
2. Keep the public transition allowlist from advertising
   `local_review_gate -> done`. This prevents agents from discovering or using
   the transition outside reconciliation.
3. Update server reconciliation tests so a runtime workflow in
   `local_review_gate` with an externally merged PR transitions to `done`.
4. Add validator tests proving:
   - reconciliation with merged PR evidence is accepted;
   - a non-reconciliation actor is rejected;
   - missing PR evidence is rejected.
5. If a reconciliation proposal is still rejected, keep it a single
   operator-visible warning path and avoid adding retries or fallback state
   changes in this tranche.

## Data Flow

Input: runtime workflow candidate with current state `local_review_gate` and
linked PR metadata. Reconciliation resolves the GitHub PR state. If the PR is
merged, it creates a `WorkflowDecision` named `reconcile_pr_merged` with a
`MarkDone` command and `github_pr` evidence. The workflow validator accepts the
decision only for the reconciliation actor and evidence-bound command payload.
The runtime store records the decision transition and updates workflow data with
`last_decision`, `external_pr_state`, `pr_number`, `pr_url`, `repo`, and
`issue_number`.

## Alternatives Considered

- Add `local_review_gate -> done` to the default allowlist. Rejected because it
  would advertise a bypass path to non-reconciliation actors.
- Suppress the rejected warning without fixing the transition. Rejected because
  it hides the stuck workflow and leaves durable state wrong.
- Move merged PR workflows to `awaiting_feedback` first. Rejected because the
  PR is already merged, so an intermediate feedback state is not user-visible
  value.

## Risks

- Security: The transition must remain reconciliation-only and evidence-bound
  so an agent cannot skip local review.
- Compatibility: Existing reconciliation states must continue to behave the
  same way.
- Performance: No new polling or external calls.
- Maintenance: The exception should share the existing PR-merge validation
  helper to avoid divergent evidence rules.

## Test Plan

- [ ] Unit tests: validator allows evidence-backed reconciliation and rejects
      non-reconciliation or missing-evidence decisions.
- [ ] Integration tests: server reconciliation marks a
      `local_review_gate` workflow `done` when GitHub reports the linked PR
      merged.
- [ ] Regression tests: existing blocked, ready-to-merge, open PR, and closed
      PR reconciliation tests still pass.
- [ ] Manual verification: inspect a reconciliation report and confirm the
      transition appears once with `from = "local_review_gate"` and `to =
      "done"`.

## Rollback Plan

Revert the validator and reconciliation test changes. Workflows that are
already reconciled to `done` remain terminal; no schema rollback is required.
