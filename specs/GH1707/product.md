# Product Spec

## Linked Issue

GH-1707

## User Problem

After workflow-runtime persistence is lost or rebuilt, GitHub intake can treat
an issue as uncovered even though an authoritative open or merged pull request
already closes it. That can enqueue duplicate implementation work, spend agent
tokens twice, and create competing pull requests. The production incident in
`majiayu000/remem` demonstrated that local runtime state alone is insufficient
as the coverage authority after recovery.

## Goals

- Reconstruct issue coverage from authoritative GitHub issue and pull-request
  facts when local runtime coverage is absent.
- Persist the recovered issue-to-PR binding and server-owned PR evidence.
- Resume the workflow in the state implied by current PR facts without
  dispatching implementation work.
- Make repeated polls and process restarts idempotent.
- Fail closed whenever the GitHub fact set is unavailable or incomplete.

## Non-Goals

- Reconstructing raw agent transcripts; GH-1704 owns that contract.
- Trusting PR title or branch-name similarity as closing evidence.
- Auto-merging, approving, or resolving review feedback.
- Treating a closed-unmerged PR as durable issue coverage.
- Repairing repositories other than the configured intake repository.

## User-Visible Behavior

1. **B-001:** With no local runtime coverage, an open issue with an
   authoritative open closing PR is covered and produces zero
   `implement_issue` jobs.
2. **B-002:** Recovery persists the issue-to-PR binding and a server-owned PR
   fact snapshot before reporting the issue covered.
3. **B-003:** An open PR is reconstructed as `pr_open`,
   `awaiting_feedback`, or `ready_to_merge` according to its current checks,
   review, and merge facts.
4. **B-004:** A merged PR plus a closed issue reconstructs terminal evidence
   and produces zero implementation or repair agent jobs.
5. **B-005:** A closed-unmerged PR does not cover the issue; a later valid PR
   may independently restore coverage.
6. **B-006:** Repeated polls and restart recovery are idempotent: they preserve
   one binding, one current workflow state, and no duplicate agent work.
7. **B-007:** A cancelled or stale local workflow cannot override a current
   authoritative closing PR; recovery replaces stale pending work with the
   reconstructed PR-owned state.
8. **B-008:** GitHub HTTP, GraphQL, pagination, parsing, or completeness
   failures are visible errors and fail closed for that intake poll. Partial
   scans must never be interpreted as “no closing PR exists.”
9. **B-009:** Closing references are repository-qualified and exact; unrelated
   issue numbers, cross-repository references, or non-closing mentions do not
   create coverage.
10. **B-010:** Concurrent/repeated recovery attempts converge without duplicate
    commands, regressed terminal state, or overwritten newer evidence.

## Acceptance Criteria

- [ ] B-001 through B-010 have deterministic tests and implementation
      evidence.
- [ ] The Remem canary scenario—empty store plus open closing PR—reconstructs
      coverage with zero `implement_issue` jobs.
- [ ] Open, feedback, ready, merged, and closed-unmerged PR states are covered.
- [ ] Restart and repeated-poll tests prove idempotency.
- [ ] GitHub failures, page-limit exhaustion, and repeated pagination URLs all
      fail closed.
- [ ] Exact-head CI, independent review, review-thread audit, and SpecRail PR
      gate pass before implementation merge.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001, B-008, and B-009. |
| Error and failure paths | Covered by B-008. |
| Authorization / permission | Covered by B-008; unavailable or unauthorized GitHub reads cannot authorize dispatch. |
| Concurrency / race / ordering | Covered by B-002 and B-010. |
| Retry / repetition / idempotency | Covered by B-006 and B-010. |
| Illegal state transitions | Covered by B-003, B-004, B-005, and B-007. |
| Compatibility / migration | Covered by B-006 and B-007 for empty or stale local stores. |
| Degradation / fallback | Covered by B-008; incomplete evidence is an error, never success. |
| Evidence and audit integrity | Covered by B-002, B-008, and B-009. |
| Cancellation / interruption / partial completion | Covered by B-007, B-008, and B-010. |

## Edge Cases

- GraphQL reports no closing link while REST finds an exact closing keyword.
- A closing-reference connection is truncated or a REST next-page URL loops.
- The matching PR is merged while GitHub still reports the issue open.
- More than one historical PR references the same issue.
- A stale cancelled workflow has pending commands when recovery begins.
- The process stops after persisting the PR fact but before the next poll.

## Rollout Notes

Recovery is activated within the existing intake coverage gate and needs no
operator migration. Errors intentionally suppress dispatch for the affected
poll and remain visible so operators can restore GitHub connectivity. Reverting
the implementation restores the prior behavior but also restores the duplicate
dispatch risk.
