# Task Plan

## Linked Issue

GH-1639

## Spec Packet

- Product: `specs/GH1639/product.md`
- Tech: `specs/GH1639/tech.md`

## Implementation Tasks

- [ ] `SP1639-T1` Owner: Codex | Done when: exact line counts, extraction boundary, helper dependencies, and name-reference search are recorded before edits | Verify: `wc -l`, `rg`, and clean diff
- [ ] `SP1639-T2` Owner: Codex | Done when: the three lifecycle fixtures move intact to the focused child module and both files are below 800 lines | Verify: move-aware diff, line counts, targeted tests, and full package tests
- [ ] `SP1639-T3` Owner: Codex | Done when: full local and remote readiness gates pass and GH-1637 can apply its scoped follow-up without VibeGuard rejection | Verify: full Clippy, workflow checks, exact-head PR evidence, and SpecRail PR gate

### SP1639-T1 — Confirm the mechanical extraction boundary

- Owner: Codex implementation agent.
- Dependencies: merged spec PR and fresh `origin/main` branch.
- Covers: B-001, B-002, B-004, B-005, B-006.
- Work:
  - record 928/772-line baselines;
  - confirm no external references to private test names;
  - confirm the three adjacent fixtures and parent helper dependencies.
- Done when: the pre-edit diff is empty and the extraction is fully bounded.
- Verify: commands in `tech.md` plus `git diff --exit-code`.

### SP1639-T2 — Extract the focused lifecycle module

- Owner: Codex implementation agent.
- Dependencies: SP1639-T1.
- Covers: B-001 through B-008.
- Work:
  - move the three fixture bodies without semantic edits;
  - add the child import and parent module registration;
  - run line-count, formatting, targeted, and package checks.
- Done when:
  - both files are below 800 lines;
  - moved fixtures pass and retain all scripts/timeouts/assertions;
  - the implementation is committed as one atomic move.
- Verify: line counts and test plan commands.

### SP1639-T3 — Complete readiness and handoff

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1639-T2.
- Covers: B-006, B-007, B-008.
- Work:
  - run full Clippy and SpecRail validators;
  - open the separate implementation PR and collect exact-head gates;
  - after merge, rebase GH-1637 and confirm its guard patch is accepted.
- Done when: remote gates allow merge and GH-1637 is unblocked.
- Verify: PR evidence and `pr_gate.py` commands from repository guidance.

## Parallelization

No parallel writable lanes. The parent deletion and child addition are one
move and require a single owner.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008}`.
- [ ] Product and task coverage sets are equal.
- [ ] Both files are below 800 lines.
- [ ] No moved assertion, timeout, or script changed.

## Handoff Notes

- Commit policy: `per_step`; the mechanical extraction is one atomic step.
- GH-1637 resumes immediately after merge.
- Standing authorization remains conditional on CI, review threads, assertion
  integrity, and SpecRail gates.
