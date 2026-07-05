# Task Plan

## Linked Issue

GH-1470

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1470-T001` Owner: `reviewer-gate` | Dependencies: none | Done when: `run_review_tick` can ask a local commit gate whether to run, skip, or fall back without direct `Command::new("git")` in `periodic_reviewer.rs` | Verify: `cargo test -p harness-server periodic_reviewer`
- [ ] `SP1470-T002` Owner: `scheduler` | Dependencies: `SP1470-T001` | Done when: unchanged projects with an existing watermark return before primary review enqueue and still preserve first-run behavior | Verify: focused periodic reviewer enqueue-decision tests
- [ ] `SP1470-T003` Owner: `observability` | Dependencies: `SP1470-T002` | Done when: local no-change skips emit structured observability with project, watermark, and reason without advancing the successful-review watermark | Verify: focused periodic reviewer skip-observability test or EventStore assertion
- [ ] `SP1470-T004` Owner: `docs` | Dependencies: `SP1470-T002` | Done when: usage and periodic-review docs accurately describe the local gate plus prompt-side fallback | Verify: `rg -n "REVIEW_SKIPPED|local gate|new commits" docs/usage-guide.md docs/periodic-review.md`
- [ ] `SP1470-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and clippy pass | Verify: `cargo test -p harness-server periodic_reviewer`, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1470`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

Keep this implementation serial. The enqueue decision, local gate, and
observability all touch `periodic_reviewer.rs`, and W-14 requires explicit
disjoint ownership before parallel edits.

## Verification

- `cargo test -p harness-server periodic_reviewer`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1470`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1470` for the spec PR. The implementation PR should use
`Closes #1470` after the local commit gate, observability, docs, and tests
merge.
