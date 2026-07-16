# Task Plan

## Linked Issue

GH-1646

## Spec Packet

- Product: `specs/GH1646/product.md`
- Tech: `specs/GH1646/tech.md`

## Implementation Tasks

- [ ] `SP1646-T1` Owner: Codex | Done when: the exact implementation base SHA, complete symbol inventory, formatted line counts, focused compile, and isolated-database server baseline are recorded | Verify: `cargo check -p harness-server --all-targets` and `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test -p harness-server --lib -- --test-threads=1`
- [ ] `SP1646-T2` Owner: Codex | Done when: facade, startup, query, runtime-host, and projection owners are extracted with unchanged behavior and all touched production files are at or below 800 lines | Verify: formatting, focused server check, post-format size gate, and targeted database-backed server tests
- [ ] `SP1646-T3` Owner: Codex | Done when: runtime-control, persistence, recovered-PR, and transition owners are extracted exactly once with preserved lock/cache/callback/event order | Verify: isolated-database server lib tests and the inventory/no-scope gates
- [ ] `SP1646-T4` Owner: Codex | Done when: every invariant has fresh local, exact-head CI, review-thread, and SpecRail gate evidence for the separate implementation PR | Verify: workspace Clippy, serial database-backed workspace tests, SpecRail checks, and PR gate

### SP1646-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1646 spec PR; fresh implementation worktree from
  current `origin/main`; isolated disposable PostgreSQL database available.
- Covers: B-001, B-002, B-003, B-007, B-008, B-009, B-010.
- Work:
  - record the base commit, all root types, inherent methods, free functions,
    helpers, line counts, and current call-site imports;
  - create only `harness_codex_gh1646_test` on the existing local server;
  - run the focused compile and serial isolated-database server baseline;
  - stop implementation if fresh product code is red and diagnose the failure.
- Done when:
  - the baseline SHA and complete inventory are preserved in the handoff;
  - focused compile and database-backed server lib tests pass;
  - an upstream failure, if any, is exposed rather than hidden by extraction.
- Verify:
  - `git rev-parse HEAD`;
  - `rg -n '^[[:space:]]*(pub(\\(crate\\))? )?(async )?fn |^pub (struct|enum)|^impl ' crates/harness-server/src/task_runner/store.rs`;
  - `cargo check -p harness-server --all-targets`;
  - the isolated-database server command above.

### SP1646-T2 — Extract facade, startup, read, claim, and projection owners

- Owner: Codex implementation agent.
- Dependencies: SP1646-T1.
- Covers: B-001, B-002, B-003, B-004, B-005, B-007, B-008, B-009.
- Work:
  - establish the child-module wiring while retaining stable types and fields
    in the root facade;
  - move construction/startup, lookup/duplicate/paging, runtime-host claim,
    and projection/metric functions as complete units;
  - retain exact SQL, cache precedence, replay ordering, lease predicates,
    imports, signatures, and error behavior;
  - use this complete group to bring the formatted root below 800 lines.
- Done when:
  - every moved symbol has exactly one owner and existing callers compile;
  - root and all touched production files are at or below 800 lines;
  - focused checks and relevant database-backed tests pass;
  - the verified step is committed before SP1646-T3.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - post-format inventory and line-count commands from `tech.md`;
  - targeted `harness-server` tests covering startup, queries, and claims.

### SP1646-T3 — Extract control, persistence, recovery, and transitions

- Owner: Codex implementation agent.
- Dependencies: SP1646-T2.
- Covers: B-001, B-002, B-004, B-005, B-006, B-007, B-008, B-009.
- Work:
  - move stream/abort/rate-limit control, artifacts/prompts/checkpoints/task
    persistence, recovered-PR validation, and mutation/terminalization groups;
  - preserve complete function bodies, lock scope, cache/persist rollback,
    event and callback order, stream cleanup, logging, and return values;
  - re-export moved transition functions only as narrowly needed by
    `task_runner/mod.rs` and avoid any new external API.
- Done when:
  - the full pre/post symbol inventory matches exactly once;
  - root and every touched production file remain below the ceiling;
  - the isolated-database server lib suite passes;
  - no manifest, lockfile, migration, task model, persisted format, or public
    wire-format path changed;
  - the verified step is committed before final readiness work.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test -p harness-server --lib -- --test-threads=1`;
  - inventory, size, scope, and movement-aware diff checks from `tech.md`.

### SP1646-T4 — Prove implementation and PR readiness

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1646-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010.
- Work:
  - run the complete deterministic verification matrix on the final commit;
  - compare the implementation against GH-1646 and all three spec documents;
  - push and open the separate implementation PR with reason, plan, risks,
    inventory, line counts, and verification evidence;
  - wait for exact-head CI and required review, audit review threads, run the
    SpecRail PR gate, and merge only under the standing authorization;
  - drop only `harness_codex_gh1646_test` after local verification is complete.
- Done when:
  - every B-001 through B-010 mapping has fresh evidence;
  - local and exact-head remote gates pass without unresolved actionable review;
  - the PR contains only the approved mechanical extraction;
  - the authorized merge and disposable-database cleanup are verified.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-server --all-targets`;
  - isolated-database server lib command above;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test --workspace -- --test-threads=1`;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1646`;
  - `python3 checks/github_pr_evidence.py --github-repo majiayu000/harness --pr <pr-number> --json > /tmp/pr-<pr-number>-evidence.json`;
  - `python3 checks/pr_gate.py --repo . --evidence /tmp/pr-<pr-number>-evidence.json --json`.

## Parallelization

No parallel writable lanes. Every extraction step touches the shared root
facade and sibling visibility, so SP1646-T1 through SP1646-T4 execute
sequentially in one implementation worktree. Independent read-only review may
run after a commit but must not edit shared files.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Product and task coverage sets are equal.
- [ ] Each implementation step is committed only after its mapped checks pass.
- [ ] Final local tests use the named isolated database with the pool cap.
- [ ] Final remote evidence belongs to the exact implementation PR head SHA.

## Handoff Notes

- Commit policy: `per_step` for SP1646-T2 and SP1646-T3; verification-only
  work may use a final commit only when it changes tracked artifacts.
- The spec PR contains no production code and does not close GH-1646.
- After spec approval, transition the issue from `ready_to_spec` to
  `ready_to_implement` before production edits.
- The maintainer supplied standing authorization for issue/spec/implementation
  creation, isolated database setup/cleanup, and merges after required gates.
  This does not waive CI, review, review-thread, security, or SpecRail gates.
- Existing user-owned changes in the original checkout remain out of scope.
