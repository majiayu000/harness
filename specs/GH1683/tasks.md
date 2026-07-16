# Task Plan

## Linked Issue

GH-1683

## Spec Packet

- Product: `specs/GH1683/product.md`
- Tech: `specs/GH1683/tech.md`

## Implementation Tasks

- [ ] `SP1683-T1` Owner: Codex | Done when: exact-base boundaries, ordered test inventory, duplicate/overlap checks, compile, 22 focused tests, and VibeGuard baseline are recorded | Verify: commands in T1 below
- [ ] `SP1683-T2` Owner: Codex | Done when: the 11 final readiness-evidence tests are moved mechanically, exact retained/moved comparisons and namespace expectations pass, both files are within the ceiling, and all 22 tests pass | Verify: commands in T2 below
- [ ] `SP1683-T3` Owner: Codex | Done when: local, exact-head CI, review-thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T3 below

### SP1683-T1 — Capture the exact implementation baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1683 specification PR and a fresh worktree from
  current `origin/main`.
- Covers: B-001 through B-006.
- Work:
  - record the exact base SHA, line boundary, ordered 22-test inventory, and
    current test paths;
  - confirm the child target does not exist and no duplicate issue or open PR
    overlaps either approved path;
  - run the all-target check and 22 focused tests;
  - record the manual VibeGuard baseline and stop if any baseline is red.
- Done when:
  - exact inventories and baseline output are preserved;
  - all 22 tests pass;
  - the planned two-file scope is unambiguous.
- Verify:
  - `git rev-parse HEAD`;
  - `wc -l crates/harness-workflow/src/runtime/tests/pr_repair_evidence.rs`;
  - inventory and boundary searches from `tech.md`;
  - `cargo check -p harness-workflow --all-targets`;
  - `cargo test -p harness-workflow pr_repair_evidence`.

### SP1683-T2 — Extract the final readiness-evidence test group

- Owner: Codex implementation agent.
- Dependencies: SP1683-T1.
- Covers: B-001 through B-005.
- Work:
  - retain exact-base lines 1-448 and append `mod readiness_evidence;`;
  - create the child file with `use super::*;` and exact-base lines 449-833;
  - preserve every attribute, name, body, assertion, helper call, artifact,
    signal, JSON value, string, and non-whitespace Rust token without semantic
    edits;
  - prove exact retained and moved content, ordered inventory, documented path
    changes, line ceilings, and approved two-file scope;
  - run focused and full workflow tests before committing.
- Done when:
  - the root and child files are each at most 800 formatted lines;
  - all 22 tests appear exactly once and all documented paths are correct;
  - exact movement and verification pass;
  - no file outside the approved pair changes.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo check -p harness-workflow --all-targets`;
  - focused and full workflow tests from `tech.md`;
  - exact root, exact child, inventory, namespace, size, and scope checks.

### SP1683-T3 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1683-T2.
- Covers: B-001 through B-007.
- Work:
  - compare the implementation with the issue and complete spec packet;
  - run the deterministic verification matrix at final head;
  - open the separate implementation PR with reason, plan, movement evidence,
    namespace change, scope, risk, and verification;
  - wait for exact-head CI and Gemini review, address valid findings, audit all
    review threads, run the required SpecRail PR gate, and merge only under the
    standing authorization;
  - remove the worktree and any disposable resources.
- Done when:
  - B-001 through B-007 have fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - authorized merge, issue closure, and cleanup are verified.
- Verify:
  - format, check, focused/full workflow test, workspace Clippy/test, SpecRail,
    exact-head CI, review-thread, and required PR gate commands from this packet.

## Parallelization

No parallel writable lanes are planned. The two files form one Rust test module
namespace and will be moved, verified, and committed sequentially.

## Verification

- [ ] Global and GH-1683 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-007 map to tasks and verification.
- [ ] Exact retained/moved content and ordered test inventory match the base.
- [ ] Both Rust files are at most 800 formatted lines.
- [ ] Focused, workflow, workspace, Clippy, VibeGuard, CI, review-thread, and PR
      gates pass on the exact implementation head.

## Handoff Notes

- Exact issue/spec base: `02fba1d27c2272d2735c95ede29fcbf447aa8f3a`.
- Baseline: 833 lines, 22 tests, retained lines 1-448, moved lines 449-833,
  and all 22 focused tests passing.
- Preserve test bodies exactly; only the 11 moved logical paths gain
  `readiness_evidence`.
- Route any production or behavioral finding to a separate issue/spec cycle.
