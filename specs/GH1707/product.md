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

- Reconstruct issue coverage from GitHub GraphQL
  `issue.closedByPullRequestsReferences` and complete pull-request facts when
  local runtime coverage is absent.
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

1. **B-001:** With no local runtime coverage, an open issue with an eligible
   active PR in the complete GitHub GraphQL
   `issue.closedByPullRequestsReferences` connection is covered and produces
   zero `implement_issue` jobs.
2. **B-002:** Recovery persists the issue-to-PR binding and a server-owned PR
   fact snapshot before reporting the issue covered.
3. **B-003:** Recovery applies this exact state matrix: waiting checks or
   mergeability becomes `pr_open`; CI or review repair becomes
   `awaiting_feedback`; a ready snapshot becomes `quality_gate_pending` and
   starts the quality gate; only a successful quality gate may later advance
   it to `ready_to_merge`.
4. **B-004:** A merged PR plus a closed issue reconstructs terminal evidence
   and produces zero implementation or repair agent jobs.
5. **B-005:** Neither a closed-unmerged PR nor a merged PR while the issue is
   still open is durable coverage; a later eligible active PR, or a merged PR
   observed with the issue closed, may independently restore coverage.
6. **B-006:** Repeated polls and restart recovery are idempotent: they preserve
   one binding, one current workflow state, and no duplicate agent work.
7. **B-007:** A cancelled or stale local workflow cannot override a current
   authoritative closing PR; recovery replaces stale pending work with the
   reconstructed PR-owned state.
8. **B-008:** GitHub HTTP, GraphQL, pagination, parsing, or completeness
   failures are visible errors and fail closed for that intake poll. Partial
   scans must never be interpreted as “no closing PR exists.”
9. **B-009:** The complete, repository-qualified GraphQL issue-link connection
   is the only closing-relation authority. A linked PR needs no closing keyword
   in its title or body; REST keyword matches, unrelated issue numbers,
   cross-repository links, and non-closing mentions without an authoritative
   issue link do not create coverage.
10. **B-010:** After complete snapshots are collected for every same-repository
    linked candidate, selection is independent of GraphQL/API order: when the
    issue is closed, eligible merged candidates outrank eligible active
    candidates; otherwise only eligible active candidates qualify; the highest
    PR number wins within the selected class. Barrier-synchronized concurrent
    and repeated attempts must converge on that candidate without duplicate
    commands, regressed terminal state, or overwritten newer evidence.

## Acceptance Criteria

- [ ] B-001 through B-010 have deterministic tests and implementation
      evidence.
- [ ] The Remem canary scenario—empty store plus open closing PR—reconstructs
      coverage with zero `implement_issue` jobs.
- [ ] Open, feedback, ready, merged, and closed-unmerged PR states are covered.
- [ ] A ready snapshot first reconstructs `quality_gate_pending`; a passing
      quality gate is the only tested path from that state to `ready_to_merge`.
- [ ] Merged-plus-open is tested as uncovered, while merged-plus-closed is
      tested as terminal `done` coverage.
- [ ] Reversed GraphQL candidate orders select the same precedence class and
      highest PR number.
- [ ] Restart and repeated-poll tests prove idempotency.
- [ ] A barrier-controlled concurrency test proves simultaneous recovery
      attempts converge on one binding and one deduplicated command set.
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
| Concurrency / race / ordering | Covered by B-002 and B-010, including order-independent candidate arbitration and a barrier-controlled race. |
| Retry / repetition / idempotency | Covered by B-006 and B-010. |
| Illegal state transitions | Covered by B-003, B-004, B-005, and B-007. |
| Compatibility / migration | Covered by B-006 and B-007 for empty or stale local stores. |
| Degradation / fallback | Covered by B-008; incomplete evidence is an error, never success. |
| Evidence and audit integrity | Covered by B-002, B-008, and B-009. |
| Cancellation / interruption / partial completion | Covered by B-007, B-008, and B-010. |

## Edge Cases

- A complete GraphQL issue-link connection reports no closing link while REST
  finds an exact closing keyword; recovery remains `Uncovered` and does not
  consult REST as an alternate authority.
- The GraphQL closing-reference connection is truncated, exceeds its page
  limit, repeats an unusable cursor, or changes issue state during pagination.
- The matching PR is merged while GitHub still reports the issue open.
- Multiple linked PRs arrive in opposite API orders, including merged, active,
  and closed-unmerged candidates with different PR numbers.
- A stale cancelled workflow has pending commands when recovery begins.
- The process stops after persisting the PR fact but before the next poll.
- Two recovery attempts reach candidate selection and persistence at the same
  barrier before either one commits.

## Rollout Notes

Recovery is activated within the existing intake coverage gate and needs no
operator migration. Errors intentionally suppress dispatch for the affected
poll and remain visible so operators can restore GitHub connectivity. Reverting
the implementation restores the prior behavior but also restores the duplicate
dispatch risk.
