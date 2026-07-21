# Task Plan

## Linked Issue

GH-1707

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1707-T1` Owner: implementation agent | Done when: authoritative candidate lookup fails closed and accepts only exact links | Verify: focused lookup and pagination tests below
- [ ] `SP1707-T2` Owner: implementation agent | Done when: workflow ownership and PR facts reconstruct without duplicate work | Verify: recovery state matrix tests below
- [ ] `SP1707-T3` Owner: verification owner | Done when: restart and repeated polls preserve one binding and zero agent jobs | Verify: restart and merged-terminal tests below
- [ ] `SP1707-T4` Owner: coordinator | Done when: all local and exact-head remote gates pass | Verify: repository, CI, review-thread, and PR-gate commands below

### SP1707-T1 — Collect and validate authoritative GitHub coverage

- Owner: implementation agent.
- Dependencies: merged GH-1707 spec PR.
- Covers: B-001, B-005, B-008, B-009.
- Work: enumerate exact closing candidates, fetch complete issue/PR snapshots,
  and fail closed on transport, parsing, pagination, or completeness failure.
- Done when: only exact repository-qualified candidates can produce coverage,
  and incomplete scans return errors.
- Verify: PR-discovery, lookup-failure, page-limit, repeated-URL, and
  closed-unmerged focused tests from `tech.md`.

### SP1707-T2 — Reconstruct durable workflow ownership

- Owner: implementation agent.
- Dependencies: SP1707-T1.
- Covers: B-002, B-003, B-004, B-007, B-010.
- Work: persist the PR snapshot and binding, derive the workflow state, cancel
  stale implementation commands, and enqueue only state-appropriate deduped
  follow-up commands.
- Done when: open, feedback, ready, and merged facts reconstruct the expected
  workflow state with no competing agent work.
- Verify: open-state matrix, merged-terminal, stale-work, and persistence tests
  from the Product-to-Test Mapping.

### SP1707-T3 — Prove restart and repeated-poll idempotency

- Owner: verification owner.
- Dependencies: SP1707-T2.
- Covers: B-001, B-002, B-004, B-006, B-010.
- Work: execute recovery twice, reopen the store, repeat intake, and inspect
  commands, bindings, facts, and terminal evidence.
- Done when: every repetition preserves one state/binding and zero duplicate
  implementation jobs.
- Verify: `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart` and merged-terminal recovery test.

### SP1707-T4 — Complete repository and PR gates

- Owner: coordinator; independent reviewer owns review verdict.
- Dependencies: SP1707-T3.
- Covers: B-001 through B-010.
- Work: run package checks, workspace clippy, PostgreSQL-backed tests when an
  isolated database is configured, SpecRail checks, exact-head CI, Gemini,
  independent review, thread audit, and PR gate.
- Done when: all available deterministic gates pass, any DB deferral is
  explicit, and the final implementation PR is merged only with current green
  evidence.
- Verify: commands in `tech.md` and exact-head GitHub evidence.

## Parallelization

The recovery transaction and candidate-validation behavior share acceptance
state and should be implemented serially. Independent review is read-only and
may run after the implementation head is pushed.

## Verification

- [ ] Product invariant set equals task coverage union: B-001 through B-010.
- [ ] Global and GH-1707 SpecRail checks pass.
- [ ] Focused fail-closed and state-recovery tests pass.
- [ ] Workspace clippy and exact-head CI pass.
- [ ] Gemini and independent review have no unresolved active finding.
- [ ] PR gate is `allowed` at the exact merge head.

## Handoff Notes

- Production evidence is the Remem #882 / PR #887 persistence-loss incident
  recorded in GH-1707.
- Never convert GitHub lookup uncertainty into `Uncovered`.
- The implementation PR is #1709; its pagination correction must remain on the
  reviewed exact head.
