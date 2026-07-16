# Task Plan

## Linked Issue

GH-1637

## Spec Packet

- Product: `specs/GH1637/product.md`
- Tech: `specs/GH1637/tech.md`

## Implementation Tasks

- [ ] `SP1637-T1` Owner: Codex | Done when: parallel, coordinated, exact, diagnostic-timing, and host-load evidence identify pre-marker scheduling as the failure phase | Verify: preserved command outputs and clean pre-edit diff
- [ ] `SP1637-T2` Owner: Codex | Done when: GH-1639's compliant Claude lifecycle module is merged and GH-1637 rebases onto current `origin/main` | Verify: line counts, merged PR evidence, and route gate
- [ ] `SP1637-T3` Owner: Codex | Done when: three root-exit fixtures observe descendant startup before applying the unchanged five-second completion deadline | Verify: targeted tests, semantic diff review, and package tests
- [ ] `SP1637-T4` Owner: Codex | Done when: three package runs, ordinary workspace tests, full Clippy, exact-head CI/review evidence, and SpecRail PR gate pass | Verify: commands in `tech.md` and PR evidence

### SP1637-T1 — Preserve the corrected root-cause evidence

- Owner: Codex implementation agent.
- Dependencies: original GH-1637 spec and failed implementation experiment.
- Covers: B-001, B-002, B-003, B-005, B-007, B-008.
- Work:
  - retain the two original full-suite failures;
  - retain the failed shared-lock and exact-test reproductions;
  - retain temporary timing evidence showing 0–14 ms cleanup after variable
    pre-marker startup delay;
  - remove all diagnostic code before implementation.
- Done when: one evidence-backed hypothesis remains and the source diff is
  clean before the corrected implementation.
- Verify: `git diff --exit-code` after diagnostic removal.

### SP1637-T2 — Land the file-size prerequisite

- Owner: Codex implementation agent.
- Dependencies: GH-1639 spec and implementation PRs.
- Covers: B-006, B-008.
- Work:
  - merge the byte-equivalent Claude lifecycle extraction;
  - rebase from fresh `origin/main`;
  - confirm all touched files remain below 800 lines.
- Done when: VibeGuard accepts scoped edits without bypass.
- Verify: line counts and GH-1639 merge evidence.

### SP1637-T3 — Anchor cleanup deadlines to descendant startup

- Owner: Codex implementation agent.
- Dependencies: SP1637-T1 and SP1637-T2.
- Covers: B-003, B-004, B-005, B-006, B-007, B-008.
- Work:
  - spawn the existing Codex streamed, Codex buffered, and Claude streamed
    root-exit executions as observable tasks;
  - bound and verify descendant marker startup;
  - apply the existing five-second timeout only after marker observation;
  - preserve scripts, response and marker assertions, final delayed safety
    checks, cancellation/drop tests, and production code.
- Done when: targeted filters pass repeatedly and the diff contains no lock,
  timeout increase, skip, retry, manifest, hook, or production edit.
- Verify: formatting, targeted tests, and semantic diff review.

### SP1637-T4 — Prove repeated readiness and merge gates

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1637-T3.
- Covers: B-001 through B-010.
- Work:
  - run three default-parallel package suites and the ordinary workspace suite;
  - verify the deterministic GH-1635 hook contract and full Clippy;
  - open the separate implementation PR and collect exact-head CI, review
    threads, merge state, and PR-gate evidence.
- Done when: all local and remote gates pass without stopping unrelated
  operator workloads or bypassing selected tests.
- Verify: all commands in the `tech.md` test plan and repository PR gate.

## Parallelization

No parallel writable lanes. GH-1639 is an ordered prerequisite; the three
fixture changes form one timing contract and have one integration owner.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Product and task coverage sets are equal.
- [ ] Three root-exit fixtures anchor the deadline after startup.
- [ ] Five-second deadlines, scripts, and final assertions remain unchanged.
- [ ] Repeated parallel and workspace commands pass under current host load.

## Handoff Notes

- Commit policy: `per_step`; corrected fixture timing is one atomic tested step.
- The original shared-lock implementation is abandoned based on fresh evidence.
- GH-1635 resumes after this repair merges.
- Standing authorization remains conditional on CI, review threads, assertion
  integrity, and SpecRail gates.
