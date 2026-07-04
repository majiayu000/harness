# Product Spec

## Linked Issue

GH-1449

## User Problem

One issue gets exactly one agent attempt. For high-value issues or issues that
already failed once, a single attempt wastes wall-clock time on retry loops,
while the frontier pattern fans out N independent candidates and keeps the
best. Harness has parallel tasks across issues but no parallel candidates for
one issue.

## Goals

- Opt-in fan-out: one issue → N (2–3) candidate runs in isolated workspaces.
- Deterministic, evidence-driven selection of exactly one winner.
- Full cost attribution per candidate.

## Non-Goals

- N > 3, tournaments, or iterative candidate breeding.
- Merging diffs from multiple candidates.
- Changing default single-candidate behavior or circuit-breaker semantics
  (GH-1430/#1446).
- LLM judgment as the primary selector.

## Behavior Invariants

1. Fan-out triggers only on explicit opt-in (issue label or per-project
   config); default remains one candidate.
2. Candidates run in disjoint worktrees/branches with per-candidate budget
   caps; no shared writable files (W-14).
3. The selection gate ranks candidates on hard evidence in fixed priority:
   validation commands green > CI conclusion > quality gate score > diff
   scope sanity; an LLM tiebreak, if ever added, applies only to exact ties
   and is recorded as such.
4. Exactly one PR results per issue: the winner's PR is opened (or the
   existing PR updated); losing candidates leave no open PRs, remote
   branches, or leaked workspaces.
5. The selection decision persists with per-candidate evidence and is
   reproducible from that evidence alone.
6. Losing trajectories are retained as run evidence (and feed GH-1448 memory
   when enabled), not discarded.
7. If all candidates fail, the workflow fails with per-candidate failure
   classes; it never silently falls back to picking a failing candidate.
8. Candidate spend is attributed per candidate in usage monitoring; the
   issue-level total equals the sum of candidates.

## Acceptance Criteria

- [ ] Label/config opt-in dispatches N candidates under one workflow.
- [ ] Selection decision record contains ranked evidence and is replayable.
- [ ] Exactly-one-PR invariant holds in success, partial-failure, and
      all-fail scenarios (tests).
- [ ] Usage monitor shows per-candidate token/cost rows.

## Edge Cases

- Two candidates produce identical evidence scores — deterministic tiebreak
  (e.g. smallest diff, then earliest completion) documented and tested.
- One candidate stalls (see GH-1437) — selection proceeds when remaining
  candidates reach terminal state and the stalled one times out.
- Winner's PR creation fails — runner-up is promoted; decision record shows
  the promotion chain.
- Concurrency limits reached — candidates queue like normal tasks; fan-out
  never bypasses project `max_concurrent`.

## Rollout Notes

Builds on `docs/parallel-pr-repair-bakeoff-spec.md`. Ship behind
`candidates.enabled` + `candidates.n` config; recommend first enabling on the
harness repo itself with n=2.
