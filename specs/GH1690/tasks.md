# Task Plan

## Linked Issue

GH-1690

## Spec Packet

- Product: `specs/GH1690/product.md`
- Tech: `specs/GH1690/tech.md`

## Implementation Tasks

- [ ] `SP1690-T1` Owner: Codex | Done when: exact-base boundaries, ordered test inventory, duplicate search, compile, 47 focused reducer tests, and VibeGuard baseline are recorded | Verify: commands in T1 below
- [ ] `SP1690-T2` Owner: Codex | Done when: the final 10 quality-gate tests move mechanically to the sibling include, exact comparisons and path expectations pass, both files are within the ceiling, and all tests pass | Verify: commands in T2 below
- [ ] `SP1690-T3` Owner: Codex | Done when: local, exact-head CI, review-thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T3 below

### SP1690-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1690 specification PR and a fresh worktree from
  current `origin/main`.
- Covers: B-001 through B-007.
- Work:
  - record the exact base SHA, line boundary, ordered 23-test inventory, and
    current logical test paths;
  - confirm the sibling target does not exist and no duplicate issue or open PR
    overlaps any approved path;
  - run the all-target check and 47 focused reducer tests;
  - record the manual VibeGuard baseline and stop if any baseline is red.
- Done when:
  - exact inventories and baseline output are preserved;
  - all 47 focused reducer tests pass;
  - the planned three-file scope is unambiguous.
- Verify:
  - record `BASE="$(git rev-parse HEAD)"`;
  - set `SOURCE=crates/harness-workflow/src/runtime/tests/completion_reducer_quality.rs`
    and
    `MOVED=crates/harness-workflow/src/runtime/tests/completion_reducer_quality_gate.rs`;
  - `test ! -e "$MOVED"`;
  - `test "$(wc -l < "$SOURCE" | tr -d ' ')" = 896`;
  - `test "$(sed -nE 's/^(async )?fn ([A-Za-z0-9_]+)\(.*/runtime::tests::\2/p' "$SOURCE" | wc -l | tr -d ' ')" = 23`;
  - `test "$(sed -n '499p' "$SOURCE")" = '#[test]'`;
  - `test "$(sed -n '500p' "$SOURCE")" = 'fn quality_gate_run_decision_starts_runtime_activity() {'`;
  - `cargo check -p harness-workflow --all-targets`;
  - `cargo test -p harness-workflow completion_reducer`;
  - `cargo test -p harness-workflow quality_gate_run_decision_starts_runtime_activity`.

### SP1690-T2 — Extract the final quality-gate test group

- Owner: Codex implementation agent.
- Dependencies: SP1690-T1.
- Covers: B-001 through B-006.
- Work:
  - retain exact-base lines 1-498;
  - create the sibling include file from exact-base lines 499-896;
  - add exactly one adjacent include to `runtime/tests.rs`;
  - preserve every attribute, name, body, assertion, helper call, artifact,
    signal, JSON value, string, non-whitespace Rust token, and logical test
    path without semantic edits;
  - prove exact retained and moved content, ordered inventory, unchanged paths,
    line ceilings, include adjacency, and approved three-file scope;
  - run focused and full workflow tests before committing.
- Done when:
  - the retained and sibling files are each at most 800 formatted lines;
  - all 23 tests appear exactly once and every logical path is unchanged;
  - exact movement and verification pass;
  - no file outside the approved three changes.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - both focused test commands and the full workflow test from `tech.md`;
  - the executable retained, moved, include-only, and ordered logical-path
    comparisons from `tech.md`;
  - line-count and approved-scope checks.

### SP1690-T3 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1690-T2.
- Covers: B-001 through B-008.
- Work:
  - compare the implementation with the issue and complete spec packet;
  - run the deterministic verification matrix at final head;
  - open the separate implementation PR with reason, plan, movement evidence,
    unchanged paths, scope, risk, and verification;
  - wait for exact-head CI and Gemini review, address valid findings, audit all
    review threads, run the required SpecRail PR gate, and merge only under the
    standing authorization;
  - remove the worktree and any disposable resources.
- Done when:
  - B-001 through B-008 have fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - authorized merge, issue closure, and cleanup are verified.
- Verify:
  - format, check, focused/full workflow test, workspace Clippy/test, SpecRail,
    exact-head CI, review-thread, and required PR gate commands from this
    packet.

## Parallelization

No parallel writable lanes are planned. The three files form one direct
`runtime::tests` namespace and will be moved, verified, and committed
sequentially.

## Verification

- [ ] Global and GH-1690 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-008 map to tasks and verification.
- [ ] Exact retained/moved content and ordered 23-test inventory match the base.
- [ ] Both Rust test files are at most 800 formatted lines.
- [ ] All logical test paths remain unchanged.
- [ ] Focused, workflow, workspace, Clippy, VibeGuard, CI, review-thread, and PR
      gates pass on the exact implementation head.

## Handoff Notes

- Exact issue/spec base: `888c64a48995b4fc89532b06434b562960c7f4b9`.
- Baseline: 896 lines, 23 tests, retained lines 1-498, moved lines 499-896,
  and 47 focused reducer tests passing.
- Preserve test bodies and logical paths exactly; use sibling `include!`, not a
  nested module.
- Route any production or behavioral finding to a separate issue/spec cycle.
