# Task Plan

## Linked Issue

GH-1462

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1462-T001` Owner: `inventory-baseline` | Dependencies: none | Done when: pre-split `cargo test -p harness-workflow -- --list` output is captured and sorted into an untracked local artifact | Verify: sorted baseline file exists and includes the current runtime tests
- [ ] `SP1462-T002` Owner: `common-fixtures` | Dependencies: `SP1462-T001` | Done when: shared runtime test imports, instance builders, enqueue helpers, completion event helpers, validation helpers, and test executors live in a common module without duplicate copies | Verify: `cargo check -p harness-workflow --tests`
- [ ] `SP1462-T003` Owner: `decision-and-reducer-split` | Dependencies: `SP1462-T002` | Done when: decision-builder and runtime-completion reducer tests move into focused modules with unchanged test names and assertions | Verify: post-move test inventory diff for moved tests is empty
- [ ] `SP1462-T004` Owner: `validator-worker-dispatcher-split` | Dependencies: `SP1462-T002` | Done when: decision validator, runtime worker, runtime command dispatcher, and remaining command/store tests move into focused modules with unchanged behavior | Verify: post-move test inventory diff for moved tests is empty
- [ ] `SP1462-T005` Owner: `file-size-policy` | Dependencies: `SP1462-T003`, `SP1462-T004` | Done when: root `runtime/tests.rs` is thin, every runtime test module is about 1,200 lines or less, and `CLAUDE.md` no longer grants the old 6,500-line exemption | Verify: `wc -l crates/harness-workflow/src/runtime/tests.rs crates/harness-workflow/src/runtime/tests/*.rs`
- [ ] `SP1462-T006` Owner: `verification` | Dependencies: all implementation tasks | Done when: inventory, package tests, SpecRail checks, formatting, and clippy all pass | Verify: sorted `--list` diff, `cargo test -p harness-workflow`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1462`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

This is mostly mechanical but touches one test tree. Avoid multiple writers in
the same files. If parallel work is used, assign disjoint file ownership after
`common.rs` is established; otherwise keep it serial.

## Verification

- `cargo test -p harness-workflow -- --list`
- sorted pre/post test-name diff
- `cargo test -p harness-workflow`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1462`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1462` for the spec PR. The implementation PR should use
`Closes #1462` only after the test inventory is unchanged, the old U-16
exemption is removed or reduced, and all verification commands pass.
