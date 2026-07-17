# Task Plan

## Linked Issue

GH-1694

## Spec Packet

- Product: `specs/GH1694/product.md`
- Tech: `specs/GH1694/tech.md`

## Implementation Tasks

- [ ] `SP1694-T1` Owner: Codex | Done when: exact-base boundaries, ordered test inventory, duplicate search, compile, 18 worker tests, 12 store tests, and VibeGuard baseline are recorded | Verify: commands in T1 below
- [ ] `SP1694-T2` Owner: Codex | Done when: the final seven completion-transition tests move mechanically to the sibling include, exact comparisons and path expectations pass, both files are within the ceiling, and all tests pass | Verify: commands in T2 below
- [ ] `SP1694-T3` Owner: Codex | Done when: local, exact-head CI, review-thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T3 below

### SP1694-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1694 specification PR and a fresh worktree from
  current `origin/main`.
- Covers: B-001 through B-008.
- Work:
  - record the exact base SHA, line boundary, ordered 15-test inventory, and
    current logical test paths;
  - confirm the sibling target does not exist and no duplicate issue or open PR
    overlaps any approved path;
  - run the all-target check, 18 worker tests, and 12 store tests;
  - record the manual VibeGuard baseline and stop if any baseline is red.
- Done when:
  - exact inventories and baseline output are preserved;
  - all 30 filtered tests pass;
  - the planned three-file scope is unambiguous.
- Verify:
  - record `BASE="$(git rev-parse HEAD)"`;
  - set `SOURCE=crates/harness-workflow/src/runtime/tests/worker_lifecycle.rs`
    and
    `MOVED=crates/harness-workflow/src/runtime/tests/worker_completion_transitions.rs`;
  - set `BASE_LIST=/tmp/GH1694-base-tests.txt`;
  - `test ! -e "$MOVED"`;
  - `test "$(wc -l < "$SOURCE" | tr -d ' ')" = 953`;
  - `test "$(sed -nE 's/^async fn ([A-Za-z0-9_]+)\(.*/\1/p' "$SOURCE" | wc -l | tr -d ' ')" = 15`;
  - `test "$(rg -o 'assert(_eq|_ne)?!|expect\(' "$SOURCE" | wc -l | tr -d ' ')" = 145`;
  - `test -z "$(sed -n '458p' "$SOURCE")"`;
  - `test "$(sed -n '459p' "$SOURCE")" = '#[tokio::test]'`;
  - `test "$(sed -n '460p' "$SOURCE")" = 'async fn runtime_worker_persists_bind_pr_payload_for_pr_open_transition() -> anyhow::Result<()> {'`;
  - `cargo test -p harness-workflow --lib -- --list > "$BASE_LIST"`;
  - `cargo check -p harness-workflow --all-targets`;
  - run both focused test commands from `tech.md`.

### SP1694-T2 — Extract the completion-transition test group

- Owner: Codex implementation agent.
- Dependencies: SP1694-T1.
- Covers: B-001 through B-007.
- Work:
  - retain exact-base lines 1-457;
  - create the sibling include file from exact-base lines 458-953;
  - add exactly one adjacent include to `runtime/tests.rs`;
  - preserve every attribute, name, body, assertion, helper call, artifact,
    signal, JSON value, string, source byte, and logical test path without
    semantic edits;
  - prove exact retained and moved content, exact concatenation, ordered
    inventory, unchanged paths, 145 assertion/expectation sites, line ceilings,
    include adjacency, and approved three-file scope;
  - run focused and full workflow tests before committing.
- Done when:
  - the retained and sibling files contain 457 and 496 formatted lines,
    respectively;
  - all 15 tests appear exactly once and every logical path is unchanged;
  - exact movement and verification pass;
  - no file outside the approved three changes.
- Verify:
  - `cargo fmt --all -- --check`;
  - `git diff --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - both focused test commands and the full workflow test from `tech.md`;
  - the executable retained, moved, concatenation, include-only, ordered
    logical-path, compiled-path, assertion-occurrence, and exact
    approved-scope checks from `tech.md`;
  - exact 457/496 line counts.

### SP1694-T3 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1694-T2.
- Covers: B-001 through B-009.
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
  - B-001 through B-009 have fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - authorized merge, issue closure, and cleanup are verified.
- Verify:
  - format, diff, check, focused/full workflow test, workspace Clippy/test,
    SpecRail, exact-head CI, review-thread, and required PR gate commands from
    this packet.

## Parallelization

No parallel writable lanes are planned. The three files form one direct
`runtime::tests` namespace and will be moved, verified, and committed
sequentially. An independent read-only reviewer may inspect the spec and
implementation after each PR is opened.

## Verification

- [ ] Global and GH-1694 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-009 map to tasks and verification.
- [ ] Exact retained/moved content and ordered 15-test inventory match the
      base.
- [ ] Both Rust test files are at most 800 formatted lines.
- [ ] All logical test paths and 145 assertion/expectation sites remain
      unchanged.
- [ ] Focused, workflow, workspace, Clippy, VibeGuard, CI, review-thread, and
      PR gates pass on the exact implementation head.

## Handoff Notes

- Exact issue/spec base: `8fa59c55e9335e163df433bd06a5aef097271fb9`.
- Baseline: 953 lines, 15 tests, 145 assertion/expectation sites, retained
  lines 1-457, moved lines 458-953, 18 filtered worker tests, and 12 filtered
  store tests passing.
- Preserve test bodies and logical paths exactly; use a sibling `include!`,
  not a nested module.
- Route any production or behavioral finding to a separate issue/spec cycle.
