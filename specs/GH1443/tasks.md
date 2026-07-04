# Task Plan

## Linked Issue

GH-1443

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1443-T1` Owner: agent — `git mv` `turn_lifecycle.rs` and `helpers.rs` into `workflow_runtime_worker/turn_engine/`, fix mod wiring and all importers, duplicating (not sharing) small private helpers still needed by legacy files. Done when: boundary grep `rg "task_executor" crates/harness-server/src/workflow_runtime_worker/` is empty and the workspace builds. Verify: `cargo check -p harness-server`
- [ ] `SP1443-T2` Owner: agent — Move `validate_command_safety` (+ tests) into `command_safety.rs` owned by workspace setup; leave `post_validator.rs` legacy-only. Done when: relocated tests pass and both call sites compile. Verify: `cargo test -p harness-server command_safety`
- [ ] `SP1443-T3` Owner: agent — Add ownership doc comments (turn_engine = workflow runtime; task_executor = legacy pending #1434) and update any CLI/builder import paths. Done when: docs present and no re-export shims remain. Verify: `rg "pub use.*task_executor" crates`

## Parallelization

T2 (post_validator/command_safety files) is disjoint from T1 (turn_lifecycle/helpers moves) and can run in parallel; T3 depends on T1.

## Verification

- [ ] `SP1443-T4` Owner: agent — Full-tree gate with move-integrity checks. Done when: warning-free check + full tests green, `git log --follow` shows pure renames for both files, and no assertion was modified. Verify: `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets && cargo test --workspace`

## Handoff Notes

This PR must land BEFORE any #1434 task-layer deletion PR — it defines the deletion boundary. Evidence: `executor.rs:129` calls `run_turn_lifecycle_with_options` per runtime job; `helpers.rs:259` is also the GH-1439 fix site (coordinate: if GH-1439 lands first, the move must carry its changes verbatim). Decision: module-under-worker chosen over new crate (crate count is shrinking per #1434). thread_manager is intentionally NOT moved — it survives contraction as in-memory state.
