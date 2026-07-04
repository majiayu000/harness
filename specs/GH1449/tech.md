# Tech Spec

## Linked Issue

GH-1449

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Dispatch | `crates/harness-workflow/src/runtime/dispatcher.rs`, `worker.rs`, `job_claim.rs` | One job per issue activity | Fan-out point |
| Workspace isolation | worktree strategy per `WORKFLOW.md` (`workspace.strategy: worktree`, branch_prefix) | One worktree per workflow | Per-candidate worktrees + branch suffixes |
| Quality gate | `crates/harness-workflow/src/runtime/quality_gate.rs` | Scores a single submission | Reused as per-candidate evidence |
| Submission | `crates/harness-workflow/src/runtime/submission.rs` | Opens/updates the PR | Must gate on selection decision |
| Prior spec | `docs/parallel-pr-repair-bakeoff-spec.md` | Draft bakeoff design | Baseline to align with |
| Usage attribution | `crates/harness-server/src/handlers/usage_monitor*.rs` | Per-process token attribution | Per-candidate rows |

## Proposed Design

1. **Fan-out** — when opt-in matches, the reducer expands the implement
   activity into N candidate jobs sharing a `candidate_group_id`; each gets
   branch `harness/<issue>-c<i>` and its own worktree.
2. **Candidate contract** — candidates run the standard implement activity
   but in `submission_mode = deferred`: they push their branch and record
   evidence, without opening a PR.
3. **Selection gate** — a new reducer step runs when all candidates are
   terminal (or timed out per GH-1437 stall rules): rank by fixed evidence
   priority (validation green > CI conclusion > quality score > diff scope;
   deterministic tiebreak: smaller diff, then earlier completion). Decision
   persisted as a `candidate_selection` runtime record with full ranking
   input.
4. **Promotion** — submission step opens the PR from the winner's branch; on
   PR-creation failure, promote runner-up and append to the decision record.
5. **Cleanup** — losing branches deleted remotely, worktrees reaped via
   existing cleanup; trajectories kept as runtime evidence.
6. **Config** — `candidates.enabled`, `candidates.n` (clamp 2–3),
   `candidates.trigger_label` (default `best-of-n`), per-candidate budget cap
   derived from existing budget config divided by n unless overridden.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P2 disjoint workspaces | fan-out worktree naming | integration test: n=2, distinct paths/branches |
| P3/P5 deterministic selection | selection gate fn | unit test: fixture evidence → identical decision on rescore |
| P4 exactly one PR | submission gating + cleanup | integration tests: success / winner-PR-fail / all-fail |
| P8 cost attribution | usage rows keyed by candidate job | handler test: per-candidate rows sum to group total |

## Data Flow

Issue (opt-in) → reducer fan-out (N jobs, candidate_group_id) → agents in
disjoint worktrees → evidence rows (validation, CI, quality, diff) →
selection gate → decision record → submission (winner PR) → cleanup.

## Alternatives Considered

- Sequential retry with different agents (existing periodic_retry) — kept as
  default; fan-out is for wall-clock-critical or previously-failed issues.
- LLM-judge selection — rejected as primary (known best-of-N selector failure
  modes); evidence gates first.
- Cross-candidate diff merging — rejected: unreviewable provenance.

## Risks

- Security: N agents on one untrusted issue multiplies injected-content
  exposure — same trust controls as single runs; pairs with GH-1450 tiers.
- Compatibility: submission_mode=deferred must not disturb single-candidate
  flow — mode defaults to immediate; exhaustive-match on the new enum.
- Performance: n× token cost — per-candidate caps and opt-in only; usage
  monitor makes spend visible.
- Maintenance: reducer complexity — selection gate is a pure function with
  its own test file.

## Test Plan

- [ ] Unit tests: selection ranking, tiebreaks, promotion chain, budget split.
- [ ] Integration tests: n=2 fixture flow (success, winner-PR-failure, all-fail, one-stall).
- [ ] Manual verification: `best-of-n` label on a real harness issue with n=2; inspect decision record and single resulting PR.

## Rollback Plan

Disable via `candidates.enabled=false` (fan-out never triggers). The
candidate_selection record type is additive; deferred submission mode is
unreachable when disabled.
