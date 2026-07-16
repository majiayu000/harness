# Task Plan

## Linked Issue

GH-1633

## Spec Packet

- Product: `specs/GH1633/product.md`
- Tech: `specs/GH1633/tech.md`

## Implementation Tasks

- [ ] `SP1633-T1` Owner: Codex | Done when: the exact base SHA, method/helper/type inventory, line counts, focused compile, and isolated-database package baseline are recorded | Verify: `cargo check -p harness-workflow --all-targets` and `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`
- [ ] `SP1633-T2` Owner: Codex | Done when: facade, prompt, event, and decision ownership is extracted with unchanged API/query behavior and all touched production files are at or below 800 lines | Verify: `cargo fmt --all -- --check && cargo check -p harness-workflow --all-targets`
- [ ] `SP1633-T3` Owner: Codex | Done when: command, runtime-job, completion, and transaction-helper ownership is extracted exactly once with preserved concurrency and idempotency behavior | Verify: `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`
- [ ] `SP1633-T4` Owner: Codex | Done when: invariant mapping, full local verification, exact-head CI/review evidence, SpecRail PR gate, and authorized merge readiness are complete | Verify: `cargo clippy --workspace --all-targets -- -D warnings` and `HARNESS_DATABASE_URL=<isolated-url> cargo test --workspace`

### SP1633-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1633 spec PR; fresh branch from current
  `origin/main`; isolated disposable PostgreSQL database available.
- Covers: B-001, B-002, B-003, B-007, B-008, B-009, B-010.
- Work:
  - record the base commit, store method/helper/type inventory, SQL-bearing
    function ownership, line counts, and baseline diff scope;
  - run the focused compile and isolated database package baseline;
  - stop if the fresh base has a product-code failure unrelated to the planned
    extraction.
- Done when:
  - the inventory and base SHA are recorded in the implementation handoff;
  - `cargo check -p harness-workflow --all-targets` passes;
  - `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`
    provides a usable green baseline or a separately diagnosed upstream
    failure with no implementation edit started.
- Verify:
  - `git rev-parse HEAD`;
  - `rg -n '^\\s*(pub )?(async )?fn |^\\s*impl WorkflowRuntimeStore|^pub (struct|enum|use)' crates/harness-workflow/src/runtime/store.rs`;
  - focused compile and isolated database package test above.

### SP1633-T2 — Extract facade, prompt, event, and decision ownership

- Owner: Codex implementation agent.
- Dependencies: SP1633-T1.
- Covers: B-001, B-002, B-003, B-004, B-007, B-008, B-009.
- Work:
  - keep stable public types, constructors, migration selection, and re-exports
    in the root facade;
  - move prompt payload, workflow event, decision, and detail-query functions
    as complete units into cohesive store sibling modules;
  - move private row aliases with their query owners and use the narrowest
    sibling visibility for shared helpers.
- Done when:
  - all moved methods exist exactly once with unchanged signatures and query
    bodies;
  - touched files are formatted and no touched production file exceeds 800
    lines;
  - focused checks pass and the step is committed before SP1633-T3 begins.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - post-format line-count command from `tech.md`;
  - movement-aware review of `git diff --find-renames origin/main`.

### SP1633-T3 — Extract command, runtime-job, and completion ownership

- Owner: Codex implementation agent.
- Dependencies: SP1633-T2.
- Covers: B-001, B-002, B-004, B-005, B-006, B-007, B-008, B-009.
- Work:
  - move command facade operations without exceeding the existing command
    module ceiling;
  - split runtime-job enqueue/claim/events, state mutation, queries, and
    lease-owned activity completion by the ownership map in `tech.md`;
  - assign transaction-local instance/event/decision/job helpers one owner and
    remove no function or call path;
  - preserve every query, bind sequence, predicate, lock, transaction, error,
    and return value mechanically.
- Done when:
  - the root facade and every touched production file are at or below 800
    lines after formatting;
  - the complete pre/post inventory matches with one owner per entry;
  - focused compile and isolated database package tests pass;
  - the step is committed before final readiness work.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`;
  - inventory, line-count, no-manifest, and diff checks from `tech.md`.

### SP1633-T4 — Prove implementation and PR readiness

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1633-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010.
- Work:
  - run the full deterministic verification matrix on the final commit;
  - compare implementation against the issue, product spec, tech spec, and
    this task plan;
  - push the implementation branch and open the separate implementation PR;
  - collect exact-head CI, review-thread, Gemini review, merge-state, and
    SpecRail PR-gate evidence;
  - remove the isolated database only after local verification is complete.
- Done when:
  - every B-001 through B-010 mapping has fresh evidence;
  - the implementation PR contains only the planned extraction;
  - all local and exact-head remote checks pass;
  - actionable review threads are resolved and the PR gate permits the
    maintainer-authorized merge.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `HARNESS_DATABASE_URL=<isolated-url> cargo test --workspace`;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1633`;
  - `python3 checks/github_pr_evidence.py --github-repo majiayu000/harness --pr <pr-number> --json > /tmp/pr-<pr-number>-evidence.json`;
  - `python3 checks/pr_gate.py --repo . --evidence /tmp/pr-<pr-number>-evidence.json --json`.

## Parallelization

No parallel writable lanes. The extraction steps modify the shared root facade
and helper visibility, so one implementation owner executes SP1633-T1 through
SP1633-T4 sequentially. Independent advisory review may run after a commit but
must not edit the implementation worktree.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Product and task coverage sets are equal.
- [ ] Each per-step commit is created only after its mapped checks pass.
- [ ] Final local verification uses an isolated disposable database.
- [ ] Final remote evidence is collected from the exact implementation PR
      head SHA.

## Handoff Notes

- Commit policy: `per_step` for SP1633-T2 and SP1633-T3; verification-only
  handoff work may use the final implementation commit when it changes tracked
  artifacts.
- The spec PR contains no production code and does not itself claim
  `ready_to_implement`; that state follows spec approval.
- The maintainer supplied standing authorization for issue/spec/implementation
  creation, isolated test database setup/cleanup, and merges after required
  gates. This authorization does not waive CI, Gemini review, unresolved-thread,
  final-review, security, or SpecRail gate requirements.
- Existing user-owned untracked files in the original `main` checkout are out
  of scope and must remain untouched.
