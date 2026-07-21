# Task Plan

## Linked Issue

GH-1707

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1707-T1` Owner: implementation agent | Done when: GraphQL-only candidate lookup fails closed and deterministic arbitration accepts only exact issue links | Verify: authority, ordering, and pagination tests below
- [ ] `SP1707-T2` Owner: implementation agent | Done when: workflow ownership and PR facts reconstruct through the exact state matrix without duplicate work | Verify: recovery and quality-gate transition tests below
- [ ] `SP1707-T3` Owner: verification owner | Done when: restart, repeated polls, and barrier-synchronized attempts preserve one binding and one deduplicated command set | Verify: restart, merged-terminal, and concurrency tests below
- [ ] `SP1707-T4` Owner: coordinator | Done when: all local and exact-head remote gates pass | Verify: repository, CI, review-thread, and PR-gate commands below

### SP1707-T1 — Collect and validate authoritative GitHub coverage

- Owner: implementation agent.
- Dependencies: merged GH-1707 spec PR.
- Covers: B-001, B-005, B-008, B-009, B-010.
- Work: enumerate candidates only from the complete GraphQL issue-link
  connection, fetch every same-repository snapshot, reject incomplete facts,
  and select by eligibility-class precedence plus highest PR number. Do not
  consult REST keywords or depend on API order.
- Done when: only exact repository-qualified GraphQL links can produce
  coverage; forward and reverse candidate orders select the same PR; merged
  candidates outrank active candidates even while the issue remains open;
  closed-unmerged candidates remain ineligible; incomplete scans return errors.
- Verify: `cargo test -p harness-server multiple_closing_pr_candidates_are_selected_deterministically`; `cargo test -p harness-server rest_keyword_without_authoritative_issue_link_is_uncovered`; PR-discovery, lookup-failure, page-limit, repeated-cursor, merged-open terminal, and closed-unmerged focused tests from `tech.md`.

### SP1707-T2 — Reconstruct durable workflow ownership

- Owner: implementation agent.
- Dependencies: SP1707-T1.
- Covers: B-002, B-003, B-004, B-007.
- Work: persist the PR snapshot and binding, derive the workflow state, cancel
  stale implementation commands, and enqueue only state-appropriate deduped
  follow-up commands.
- Done when: waiting facts map to `pr_open`, repair facts map to
  `awaiting_feedback`, a ready snapshot maps to `quality_gate_pending`, only a
  passing quality gate advances to `ready_to_merge`, and merged-plus-closed
  or merged-plus-open maps to terminal `done`, all with no competing agent
  work. The merged-plus-open combination is recorded as issue-state propagation
  lag rather than treated as missing coverage.
- Verify: open-state matrix, quality-gate parent propagation,
  merged-plus-closed terminal, merged-plus-open terminal, stale-work, and
  persistence tests from the Product-to-Test Mapping.

### SP1707-T3 — Prove restart and repeated-poll idempotency

- Owner: verification owner.
- Dependencies: SP1707-T2.
- Covers: B-001, B-002, B-004, B-006, B-010.
- Work: execute recovery twice, reopen the store, repeat intake, then use an
  explicit barrier to hold two attempts after candidate selection and release
  them together at the persistence boundary. Inspect commands, bindings,
  facts, and terminal evidence.
- Done when: serial repetition and the controlled race preserve one selected
  binding, one current state, one deduplicated command set, and zero duplicate
  implementation jobs.
- Verify: `cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart`; `cargo test -p harness-server merged_pr_and_closed_issue_recover_terminal_coverage_without_agent_work`; `cargo test -p harness-server merged_pr_and_open_issue_recover_terminal_coverage_without_agent_work`; barrier-controlled `cargo test -p harness-server concurrent_recovery_attempts_converge_on_one_binding_and_command_set`.

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
- [ ] Forward/reverse candidate ordering and GraphQL-only authority tests pass.
- [ ] Barrier-controlled concurrent recovery test passes.
- [ ] Workspace clippy and exact-head CI pass.
- [ ] Gemini and independent review have no unresolved active finding.
- [ ] PR gate is `allowed` at the exact merge head.

## Handoff Notes

- Production evidence is the Remem #882 / PR #887 persistence-loss incident
  recorded in GH-1707.
- Never convert GitHub lookup uncertainty into `Uncovered`.
- The implementation PR is #1709; its pagination correction must remain on the
  reviewed exact head.
