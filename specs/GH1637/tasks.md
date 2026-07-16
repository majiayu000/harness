# Task Plan

## Linked Issue

GH-1637

## Spec Packet

- Product: `specs/GH1637/product.md`
- Tech: `specs/GH1637/tech.md`

## Implementation Tasks

- [ ] `SP1637-T1` Owner: Codex | Done when: the failing parallel, passing exact, and passing serial baselines are recorded on a fresh `origin/main` branch | Verify: ordinary workspace test, exact filters, and the 193-test binary with `--test-threads=1`
- [ ] `SP1637-T2` Owner: Codex | Done when: one crate-level async test-only coordination helper is used by the seven scoped lifecycle fixtures with all existing assertions unchanged | Verify: targeted diff review and `cargo test -p harness-agents --lib`
- [ ] `SP1637-T3` Owner: Codex | Done when: three consecutive default-parallel package runs and the ordinary workspace command pass without skips or thread overrides | Verify: repeated package command plus `cargo test --workspace --exclude harness-server --exclude harness-workflow --lib`
- [ ] `SP1637-T4` Owner: Codex | Done when: local readiness, exact-head CI/review evidence, SpecRail PR gate, and authorized merge readiness are complete | Verify: full Clippy, workflow checks, PR evidence, and PR gate

### SP1637-T1 — Preserve and classify the baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1637 spec PR; fresh branch from current
  `origin/main`.
- Covers: B-001, B-002, B-005, B-007, B-010.
- Work:
  - retain both full-suite timeout outputs and exact passing outputs;
  - confirm the full binary passes serially;
  - inventory every long-lived lifecycle fixture before editing.
- Done when:
  - evidence distinguishes full-suite fixture contention from a deterministic
    single-cleanup failure;
  - no file changed during baseline capture.
- Verify:
  - baseline commands listed in `tech.md`;
  - `git diff --exit-code` before implementation.

### SP1637-T2 — Add scoped async fixture coordination

- Owner: Codex implementation agent.
- Dependencies: SP1637-T1.
- Covers: B-003, B-004, B-005, B-006, B-007, B-008.
- Work:
  - add one `#[cfg(test)]` Tokio mutex helper in `harness-agents/src/lib.rs`;
  - acquire it at the start of the seven named lifecycle fixtures;
  - hold each guard through the final existing cleanup assertion;
  - change no fixture script, timeout, assertion, manifest, hook, or production
    cleanup behavior.
- Done when:
  - the package suite passes with default parallelism;
  - the implementation diff contains only the helper and guard acquisitions.
- Verify:
  - `cargo fmt --all -- --check`;
  - `cargo test -p harness-agents --lib`;
  - exact semantic diff review.

### SP1637-T3 — Prove repeated parallel stability

- Owner: Codex implementation agent.
- Dependencies: SP1637-T2.
- Covers: B-001, B-002, B-005, B-007, B-009, B-010.
- Work:
  - run the package suite three consecutive times without thread overrides;
  - run the ordinary pre-push workspace test command;
  - verify GH-1635's deterministic hook test after rebasing that branch.
- Done when:
  - every repeated and workspace run passes;
  - no skip, ignore, timeout increase, or global serialization appears.
- Verify:
  - commands in the `tech.md` test plan.

### SP1637-T4 — Complete local and remote readiness gates

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1637-T3.
- Covers: B-001 through B-010.
- Work:
  - run full workspace Clippy and SpecRail checks;
  - compare implementation to the issue and full spec packet;
  - open the separate implementation PR;
  - collect exact-head CI, review-thread, merge-state, and PR-gate evidence.
- Done when:
  - all local and remote gates pass with no unresolved actionable feedback;
  - authorized merge does not bypass required review evidence.
- Verify:
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `python3 checks/check_workflow.py --repo .`;
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1637`;
  - `python3 checks/github_pr_evidence.py --github-repo majiayu000/harness --pr <pr-number> --json > /tmp/pr-<pr-number>-evidence.json`;
  - `python3 checks/pr_gate.py --repo . --evidence /tmp/pr-<pr-number>-evidence.json --json`.

## Parallelization

No parallel writable lanes. The shared helper and both adapter test modules are
one concurrency contract and require a single integration owner. Independent
review may be read-only.

## Verification

- [ ] Product invariant set:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Task coverage union:
      `{B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010}`.
- [ ] Product and task coverage sets are equal.
- [ ] Seven lifecycle fixtures use the same async test-only guard.
- [ ] Existing fixture scripts, timeouts, and assertions are unchanged.
- [ ] Repeated parallel and workspace commands pass.

## Handoff Notes

- Commit policy: `per_step`; helper/guard implementation is one atomic tested
  step because neither half is useful alone.
- GH-1635 remains paused at real-hook readiness until this repair merges.
- Maintainer standing authorization covers issue/spec/implementation and merge
  only after required gates; it does not waive CI, review threads, assertion
  integrity, or SpecRail gates.
