# Task Plan

## Linked Issue

GH-1649

## Spec Packet

- Product: `specs/GH1649/product.md`
- Tech: `specs/GH1649/tech.md`

## Implementation Tasks

- [ ] `SP1649-T1` Owner: Codex | Done when: exact-base inventory, call sites, formatted line counts, focused compile, VibeGuard baseline, and isolated PostgreSQL test baseline are recorded | Verify: commands in T1 below
- [ ] `SP1649-T2` Owner: Codex | Done when: target loading, instance/data helpers, and command-state/suppression owners are extracted exactly once, all files are within the ceiling, and focused tests pass | Verify: commands in T2 below
- [ ] `SP1649-T3` Owner: Codex | Done when: persistence/commit owners are extracted with unchanged lifecycle behavior and the verified step is committed | Verify: commands in T3 below
- [ ] `SP1649-T4` Owner: Codex | Done when: local, exact-head CI, Gemini/thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T4 below

### SP1649-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1649 spec PR; fresh worktree from current
  `origin/main`; one disposable PostgreSQL database.
- Covers: B-001, B-003, B-004, B-005, B-006, B-007, B-009.
- Work:
  - record the exact base SHA, root/child line counts, all function/type names,
    external caller paths, imports, and private cross-group calls;
  - run duplicate-file/symbol searches before creating child modules;
  - run focused compile, VibeGuard baseline, and database-backed PR-feedback
    tests before editing;
  - stop and diagnose if fresh production code is red.
- Done when:
  - the complete baseline inventory and exact base are preserved;
  - focused compile and relevant isolated-database tests pass;
  - any upstream failure is exposed rather than hidden by extraction.
- Verify:
  - `git rev-parse HEAD`;
  - `wc -l crates/harness-server/src/workflow_runtime_pr_feedback.rs crates/harness-server/src/workflow_runtime_pr_feedback/*.rs`;
  - symbol and external-reference searches from `tech.md`;
  - `cargo check -p harness-server --all-targets`;
  - `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test -p harness-server workflow_runtime_pr_feedback -- --test-threads=1`.

### SP1649-T2 — Extract target, data, and command-state owners

- Owner: Codex implementation agent.
- Dependencies: SP1649-T1.
- Covers: B-001, B-002, B-003, B-004, B-006, B-007, B-009.
- Work:
  - move workflow definition registration, target loading, instances, IDs,
    runtime JSON, and field helpers as complete bodies;
  - move active-command checks, status predicates, and failed-child suppression
    as complete bodies;
  - preserve every query, predicate, cutoff, JSON key, default, error, and log;
  - retain helpers used by nested tests with `pub(super)` in the owner and
    explicit root `#[cfg(test)] use` imports;
  - use only narrow sibling visibility and keep external caller paths stable.
- Done when:
  - every moved symbol has one owner and the exact multiset still matches;
  - all formatted touched production files are at most 800 lines;
  - focused compile and relevant database-backed tests pass;
  - the verified extraction step is committed before T3.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - exact inventory, caller-path, size, and scope gates;
  - isolated-database PR-feedback runtime tests.

### SP1649-T3 — Extract persistence and decision-commit owners

- Owner: Codex implementation agent.
- Dependencies: SP1649-T2.
- Covers: B-001, B-002, B-003, B-004, B-005, B-007, B-009.
- Work:
  - move private `persist_*`, merge approval, decision commit, and commit-outcome
    logic as complete bodies;
  - preserve validator inputs, transition rejection, instance/command/evidence
    persistence, retry/failure handling, event order, errors, logs, and returns;
  - preserve root test-namespace access to moved persistence helpers without
    editing tests or widening production visibility;
  - retain existing facade entry points and context/outcome type paths.
- Done when:
  - exact inventories and movement review show no missing, duplicate, or
    behavior-edited body;
  - full isolated-database server library tests pass;
  - no manifest, lockfile, dependency, migration, schema, model, API, caller,
    or test file changed;
  - the verified extraction step is committed before readiness work.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test -p harness-server --lib -- --test-threads=1`;
  - exact inventory, caller, size, scope, and movement-aware diff checks.

### SP1649-T4 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1649-T3.
- Covers: B-001 through B-010.
- Work:
  - compare the implementation against the issue and all three spec files;
  - run the complete deterministic verification matrix at the final head;
  - open the separate implementation PR with reason, plan, inventory, module
    layout, risk, and verification evidence;
  - wait for exact-head CI and Gemini review, address valid findings, audit all
    review threads, run required SpecRail PR gate, and merge only under the
    standing authorization;
  - remove only the disposable database and implementation worktree.
- Done when:
  - every B-001 through B-010 item has fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - the PR contains only the approved mechanical extraction;
  - authorized merge, issue closure, and cleanup are verified.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - isolated-database PR-feedback and server test commands above;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test --workspace -- --test-threads=1`;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1649`;
  - fresh GitHub evidence and `python3 checks/pr_gate.py --repo . --evidence <evidence.json> --mode required --json`.

## Parallelization

No parallel writable implementation lanes are planned. The target, command,
and persistence groups have private cross-calls in one facade, so one agent
will move and verify them sequentially. Read-only CI/review evidence collection
may overlap only after the implementation branch is pushed.

## Verification

- [ ] Global and GH-1649 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-010 map to tasks and verification.
- [ ] Exact function/type and caller inventories match the base.
- [ ] Root and all touched production modules are at most 800 formatted lines.
- [ ] Focused, server, workspace, Clippy, VibeGuard, CI, review-thread, and PR
      gates pass on the exact implementation head.

## Handoff Notes

- Exact issue/spec base: `d812d82381517fa5da77302e4429b66c3a23f126`.
- Baseline root: 1,677 lines, 47 functions, 10 types, nine external reference
  files; focused all-target compile passed.
- Preserve `pr_lifecycle_persist.rs` behavior and current nested test paths.
- This packet authorizes mechanical extraction only. Route any behavior change
  discovered during implementation to a separate issue/spec cycle.
