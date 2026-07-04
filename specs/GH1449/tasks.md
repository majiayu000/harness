# Task Plan

## Linked Issue

GH-1449

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1449-T001` Owner: workflow-submission | Done when: `submission_mode` (immediate/deferred) is threaded through the submission path with exhaustive matches and unchanged single-candidate behavior | Verify: `cargo test -p harness-workflow submission_mode`
- [ ] `SP1449-T002` Owner: workflow-reducer | Done when: opt-in fan-out creates N candidate jobs with `candidate_group_id`, per-candidate branches/worktrees, and budget split, gated by label + config | Verify: `cargo test -p harness-workflow candidate_fanout`
- [ ] `SP1449-T003` Owner: workflow-selection | Done when: the selection gate is a pure function over persisted evidence with fixed priority ranking, deterministic tiebreaks, and a persisted `candidate_selection` record including any promotion chain | Verify: `cargo test -p harness-workflow candidate_selection`
- [ ] `SP1449-T004` Owner: workflow-submission | Done when: the winner's branch produces exactly one PR, losing branches/worktrees are cleaned, and winner-PR-failure promotes the runner-up | Verify: `cargo test -p harness-workflow candidate_promotion`
- [ ] `SP1449-T005` Owner: server-usage | Done when: usage monitor attributes tokens/cost per candidate and the group total equals the candidate sum | Verify: `cargo test -p harness-server candidate_usage`
- [ ] `SP1449-T006` Owner: workflow-runtime | Done when: a stalled candidate's timeout feeds selection readiness so remaining terminal candidates are ranked without waiting forever (aligned with GH-1437) | Verify: `cargo test -p harness-workflow candidate_stall`

## Parallelization

- T001 ∥ T003 (disjoint: submission.rs vs new selection module).
- T002 after T001; T004 after T002+T003; T005 ∥ T004 (handler files); T006 last.

## Verification

- `cargo test --workspace`
- Integration: n=2 scenarios (success / winner-PR-fail / all-fail / one-stall) each preserve the exactly-one-PR invariant.
- Manual: real harness issue with the `best-of-n` label at n=2; inspect the decision record, the single PR, and per-candidate usage rows.

## Handoff Notes

- Align terminology with `docs/parallel-pr-repair-bakeoff-spec.md` and update that doc to point at this packet.
- Selection stays a pure function over persisted evidence; any future LLM tiebreak goes behind the same record contract and only on exact ties.
