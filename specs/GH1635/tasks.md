# Task Plan

## Linked Issue

GH-1635

## Spec Packet

- Product: `specs/GH1635/product.md`
- Tech: `specs/GH1635/tech.md`

## Implementation Tasks

- [ ] `SP1635-T1` Owner: Codex | Done when: exact-base failing DB-less and passing isolated-DB evidence, hook command inventory, and affected instruction anchors are recorded | Verify: targeted GH1602 reproduction and `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`
- [ ] `SP1635-T2` Owner: Codex | Done when: the pre-push hook implements the two-mode isolated test matrix with fail-fast cleanup and no test changes | Verify: `bash -n .githooks/pre-push && bash scripts/test-pre-push-hook.sh`
- [ ] `SP1635-T3` Owner: Codex | Done when: deterministic hook tests cover both modes, failures, secret non-disclosure, git-env isolation, and cleanup while scoped instruction text matches the hook | Verify: `bash scripts/test-pre-push-hook.sh` and targeted documentation diff review
- [ ] `SP1635-T4` Owner: Codex | Done when: real DB-less and isolated-DB hooks, local readiness checks, exact-head CI/review evidence, SpecRail PR gate, and authorized merge readiness are complete | Verify: `.githooks/pre-push` in both modes and `python3 checks/pr_gate.py --repo . --evidence <evidence.json> --json`

### SP1635-T1 — Capture the exact failure and passing baselines

- Owner: Codex implementation agent.
- Dependencies: merged GH-1635 spec PR; fresh branch from current
  `origin/main`; isolated disposable PostgreSQL URL available.
- Covers: B-002, B-003, B-004, B-005, B-010, B-012.
- Work:
  - record the base SHA and existing hook command/environment matrix;
  - reproduce the six GH1602 failures in a truly empty config environment;
  - prove the full workflow package passes with the isolated URL;
  - record exact documentation anchors before any high-context edit.
- Done when:
  - failure and passing outputs have one root-cause explanation;
  - no production or test file was changed during baseline capture.
- Verify:
  - targeted reproduction from GH-1635;
  - `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`;
  - `git diff --exit-code` before implementation.

### SP1635-T2 — Implement and unit-test the two-mode hook matrix

- Owner: Codex implementation agent.
- Dependencies: SP1635-T1.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010, B-011.
- Work:
  - implement invocation-local DB-less configuration isolation and cleanup;
  - split non-workflow, DB-less workflow, full workflow, and full server
    commands according to `tech.md`;
  - add the dependency-free fake-cargo contract test;
  - leave GH1602 tests and all production Rust code unchanged.
- Done when:
  - deterministic tests pass for both modes and every injected failure;
  - no sentinel URL appears in output or persistent files;
  - shell syntax passes and the implementation step is committed before docs.
- Verify:
  - `bash -n .githooks/pre-push scripts/test-pre-push-hook.sh`;
  - `bash scripts/test-pre-push-hook.sh`;
  - `git diff origin/main -- crates/harness-workflow/src/runtime/tests/remote_host_lease.rs` is empty.

### SP1635-T3 — Align scoped instructions and prove real hook modes

- Owner: Codex implementation agent.
- Dependencies: SP1635-T2.
- Covers: B-002, B-003, B-004, B-005, B-006, B-008, B-012.
- Work:
  - update only the existing pre-push guidance in `AGENTS.md`, `CLAUDE.md`, and
    `CONTRIBUTING.md`;
  - run the real hook without a DB and with the isolated DB;
  - inspect temporary-state cleanup and output redaction.
- Done when:
  - both real modes pass on the same implementation head;
  - high-context diffs contain no unrelated policy changes;
  - documentation and verification evidence are committed separately after
    their checks pass.
- Verify:
  - `.githooks/pre-push` with DB variables unset and isolated config;
  - `HARNESS_DATABASE_URL=<isolated-url> .githooks/pre-push`;
  - `rg -n 'pre-push|HARNESS_DATABASE_URL' AGENTS.md CLAUDE.md CONTRIBUTING.md`.

### SP1635-T4 — Complete local and remote readiness gates

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1635-T3.
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009,
  B-010, B-011, B-012.
- Work:
  - run the complete local verification matrix on the final commit;
  - compare the diff against the issue and full spec packet;
  - open the separate implementation PR;
  - collect exact-head CI, Gemini, review-thread, merge-state, and PR-gate
    evidence before the authorized merge.
- Done when:
  - every invariant has fresh evidence and both real hook modes pass;
  - exact-head remote gates pass with no unresolved actionable feedback.
- Verify:
  - all `tech.md` test-plan commands;
  - `python3 checks/github_pr_evidence.py --github-repo majiayu000/harness --pr <pr-number> --json > /tmp/pr-<pr-number>-evidence.json`;
  - `python3 checks/pr_gate.py --repo . --evidence /tmp/pr-<pr-number>-evidence.json --json`.

## Parallelization

No parallel writable lanes. The hook, its fake-cargo test, and high-context
documentation describe one contract and must have one integration owner.
Independent advisory review may be read-only after each commit.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011, B-012}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011, B-012}`.
- [ ] Product and task coverage sets are equal.
- [ ] GH1602 tests and production Rust files have no diff.
- [ ] Both deterministic and real hook matrices pass.
- [ ] Exact-head remote evidence is collected after the final push.

## Handoff Notes

- Commit policy: `per_step`; hook/test implementation and scoped instruction
  alignment are separate tested commits.
- The spec PR contains no hook, test, or high-context instruction change and
  does not claim `ready_to_implement` before approval.
- The maintainer standing authorization covers issue/spec/implementation,
  isolated database setup/cleanup, and merges only after required gates. It
  does not waive test integrity, Gemini, CI, review threads, or PR gates.
- This repair precedes GH-1633 implementation because a red delivery gate must
  be fixed before production refactoring.
