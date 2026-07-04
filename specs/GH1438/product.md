# Product Spec

## Linked Issue

GH-1438

## User Problem

The workflow reconciler repeatedly proposes `local_review_gate -> done` for
workflows whose PR has already been merged externally. The workflow validator
rejects that transition, so every reconciliation cycle logs the same
`TransitionNotAllowed` failure and the workflow stays in `local_review_gate`.

Operators see noisy logs instead of one terminal reconciliation result. The
workflow also remains active even though the durable GitHub PR state says the
work is complete.

## Goals

- A workflow in `local_review_gate` with evidence that its linked PR was merged
  externally reaches `done` through a legal reconciliation path.
- Reconciliation does not repeatedly emit the same invalid transition rejection
  for the same merged PR.
- The completion remains evidence-bound to GitHub PR state and the
  reconciliation actor.
- Existing local-review behavior remains unchanged for open PRs and PRs with
  requested changes.

## Non-Goals

- Allowing arbitrary agents or API callers to bypass local review by marking
  `local_review_gate` workflows `done`.
- Changing the local review gate semantics for open PRs.
- Changing PR feedback collection, quality gates, or merge authorization.
- Adding a new operator UI surface.

## User-Visible Behavior

When reconciliation observes that a linked GitHub PR is merged while the
workflow is in `local_review_gate`, Harness records the same external PR
evidence it already uses for other reconciliation transitions and moves the
workflow to `done` once.

If the linked PR is still open, closed without merge, unknown, or missing
required evidence, the workflow must not be marked done. Reconciliation should
avoid repeated invalid transition spam and should surface the stuck state with a
single clear reason when it cannot legally advance.

## Acceptance Criteria

- [ ] A `github_issue_pr` workflow in `local_review_gate` with a merged linked
      PR is reconciled to `done` with `last_decision =
      "reconcile_pr_merged"` and external PR evidence recorded in workflow
      data.
- [ ] The validator rejects non-reconciliation attempts to mark
      `local_review_gate` as `done`.
- [ ] Reconciliation does not re-log the same
      `TransitionNotAllowed: 'local_review_gate' -> 'done'` warning on every
      cycle for a workflow it cannot legally advance.
- [ ] Existing `blocked -> done`, `ready_to_merge -> done`, and open-PR
      reconciliation behavior remains unchanged.
- [ ] Tests cover the successful merged-PR path and at least one rejected
      unevidenced path.

## Edge Cases

- The workflow state changed after candidates were collected: reconciliation
  must no-op rather than applying a stale transition.
- The PR number is present but repo or issue metadata is missing: do not mark
  done unless the existing reconciliation evidence rules accept the case.
- The PR is closed but not merged: use the existing cancellation path, not
  `done`.
- The workflow is already terminal: skip without emitting a duplicate
  transition.

## Rollout Notes

This is a reconciliation correctness fix. It changes how already-merged PRs are
reflected in workflow state, but does not change merge authorization or agent
review policy. No data migration is required.
