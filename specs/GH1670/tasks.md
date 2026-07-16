# Task Plan

## Linked Issue

GH-1670

## Spec Packet

- Product: `specs/GH1670/product.md`
- Tech: `specs/GH1670/tech.md`

## Implementation Tasks

- [ ] `SP1670-T1` Owner: Codex | Done when: exact-base production/test inventories, line counts, duplicate search, focused compile, PostgreSQL tests, and VibeGuard baseline are recorded | Verify: commands in T1 below
- [ ] `SP1670-T2` Owner: Codex | Done when: the inline test block is moved mechanically to the standard child module, exact inventories and paths match, both files are within the ceiling, and tests pass | Verify: commands in T2 below
- [ ] `SP1670-T3` Owner: Codex | Done when: local, exact-head CI, review-thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T3 below

### SP1670-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1670 spec PR and a fresh worktree from current
  `origin/main`.
- Covers: B-001 through B-005.
- Work:
  - record the exact base SHA and formatted source/test line boundaries;
  - record every production item, test helper, test name, and test attribute in
    order;
  - confirm the standard child-module target does not already exist;
  - run focused compile/tests with a dedicated PostgreSQL database and a
    VibeGuard baseline before editing;
  - stop and diagnose if the fresh baseline is red.
- Done when:
  - exact inventories and baseline output are preserved;
  - `harness-workflow` all-target compile and all 15 filtered tests pass;
  - no duplicate issue, PR, child file, or symbol exists.
- Verify:
  - `git rev-parse HEAD`;
  - `wc -l crates/harness-workflow/src/plan_db.rs`;
  - production/test inventory searches from `tech.md`;
  - `cargo check -p harness-workflow --all-targets`;
  - `cargo test -p harness-workflow plan_db::tests -- --test-threads=1` with
    the dedicated database environment.

### SP1670-T2 — Extract the private child test module

- Owner: Codex implementation agent.
- Dependencies: SP1670-T1.
- Covers: B-001 through B-004.
- Work:
  - replace the inline test wrapper with `#[cfg(test)] mod tests;`;
  - move the wrapper contents to `plan_db/tests.rs` in the same order;
  - preserve `use super::*`, local imports, helpers, statics, attributes,
    names, SQL, fixtures, strings, assertions, and calls without token or
    semantic edits;
  - generate the expected child file by unindenting the exact-base test body
    and formatting it with the repository `rustfmt`; require an exact diff;
  - compare exact production/test inventories and perform a movement-aware
    diff review that permits only the reproduced whitespace reflow;
  - commit the verified relocation before PR-readiness work.
- Done when:
  - both formatted files are at most 800 lines;
  - every production and test inventory item appears exactly once;
  - the 15 focused tests retain `plan_db::tests::*` paths and pass against the
    dedicated PostgreSQL database;
  - no file outside the approved pair changes.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - focused PostgreSQL test command above;
  - exact inventory, module-path, size, scope, production-prefix, and
    canonical-rustfmt movement checks.

### SP1670-T3 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1670-T2.
- Covers: B-001 through B-006.
- Work:
  - compare the implementation with the issue and all three spec files;
  - run the complete deterministic verification matrix at final head;
  - open the separate implementation PR with reason, plan, inventory, scope,
    risk, and verification evidence;
  - wait for exact-head CI and Gemini review, address valid findings, audit all
    review threads, run the required SpecRail PR gate, and merge only under the
    standing authorization;
  - remove the implementation worktree and all disposable resources.
- Done when:
  - B-001 through B-006 have fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - only the approved test relocation is present;
  - authorized merge, issue closure, and cleanup are verified.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - focused and full `harness-workflow` library tests above;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - workspace tests under the repository test environment;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1670`;
  - fresh GitHub evidence and `python3 checks/pr_gate.py --repo . --evidence <evidence.json> --mode required --json`.

## Parallelization

No parallel writable lanes are planned. The change is one atomic relocation in
one Rust module namespace, so one implementation agent will move, verify, and
commit it sequentially.

## Verification

- [ ] Global and GH-1670 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-006 map to tasks and verification.
- [ ] Production and test inventories match the exact base.
- [ ] Both Rust files are at most 800 formatted lines.
- [ ] Focused, package, workspace, Clippy, VibeGuard, CI, review-thread, and PR
      gates pass on the exact implementation head.

## Handoff Notes

- Exact issue/spec base: `654751c3a83672bfbdcdf2c771c69a199a96de3f`.
- Baseline: 822 lines, production through line 388, inline tests from lines
  389-822, 15 focused PostgreSQL tests passing.
- Preserve the logical `plan_db::tests::*` namespace and private parent access.
- This packet authorizes test relocation only. Route any production, SQL,
  migration, or behavioral finding to a separate issue/spec cycle.
