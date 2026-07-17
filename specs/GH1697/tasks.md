# Task Plan

## Linked Issue

GH-1697

## Spec Packet

- Product: `specs/GH1697/product.md`
- Tech: `specs/GH1697/tech.md`

## Implementation Tasks

- [ ] `SP1697-T1` Owner: Codex | Done when: exact base, duplicate/overlap search, line/test/helper/assertion inventories, compiled test list, package check, 11 focused tests, and VibeGuard baseline are recorded | Verify: before-edit commands in `tech.md`
- [ ] `SP1697-T2` Owner: Codex | Done when: exact-base lines 620-832 move to the support include, exact main/support comparisons and inventories pass, both files are below the ceiling, and focused/full tests pass | Verify: after-edit and test commands in `tech.md`
- [ ] `SP1697-T3` Owner: Codex | Done when: the separate implementation PR passes exact-head CI, review-thread, independent-review, and required SpecRail gates and cleanup is verified | Verify: final PR evidence and `checks/pr_gate.py`

## Task Details

### SP1697-T1 — Capture baseline

- Dependencies: merged GH-1697 spec PR; fresh worktree from `origin/main`.
- Covers: B-001 through B-007.
- Preserve the complete `--list` output before editing.
- Stop if the base, inventories, target absence, focused tests, or package
  check differ from the packet.
- Run the package check and 11 focused module tests in the before-edit block,
  using the sanitized database-independent environment specified in
  `tech.md`.

### SP1697-T2 — Extract support block

- Dependencies: SP1697-T1.
- Covers: B-001 through B-006.
- Create the support file from exact-base lines 620-832.
- Replace only that range with the single direct include.
- Prove 688/213 lines, 11 tests, nine helpers, 43 occurrences, identical
  compiled paths, exact source ranges, and exact two-file working-tree scope.
- Run final formatting, diff, package, focused, and sanitized full workflow
  checks before commit; require configured exact-head CI for the six deferred
  PostgreSQL lease tests.

### SP1697-T3 — Prove readiness

- Dependencies: SP1697-T2.
- Covers: B-001 through B-008.
- Open a separate implementation PR with reason, plan, exact movement,
  inventory, scope, test, and rollback evidence.
- After committing, require a clean worktree and the exact two-file committed
  scope gate from `tech.md`.
- Wait for exact-head CI and advisory review, address valid findings, audit
  review threads, obtain independent read-only review, run the required PR
  gate, and merge only under standing authorization.
- Verify issue closure, branch/worktree cleanup, and refreshed `origin/main`.

## Parallelization

Writable work is serialized because both files expand into one Rust module.
An independent read-only reviewer may inspect the spec and implementation PRs.

## Verification

- [ ] Global and GH-1697 SpecRail checks pass.
- [ ] B-001 through B-008 map to tasks and executable commands.
- [ ] Exact source, line, test, helper, assertion, compiled-path, and scope
      checks pass.
- [ ] Focused/full workflow tests, workspace Clippy/tests, VibeGuard,
      exact-head CI, review threads, independent review, and PR gate pass.

## Handoff Notes

- Exact issue/spec base: `08157897b686c6ae9245d626c7d2997b93acdf27`.
- Baseline: 900 lines, 11 tests, nine helpers, and 43
  assertion/expectation occurrences.
- Extract exact lines 620-832; retain line 833's blank separator in the main
  module.
- Route behavioral or SQL findings to a separate issue/spec cycle.
