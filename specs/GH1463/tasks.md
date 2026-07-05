# Task Plan

## Linked Issue

GH-1463

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1463-T001` Owner: `module-map` | Dependencies: none | Done when: the current `background.rs` functions and types are mapped to focused child modules with no planned behavior changes | Verify: module list matches the concern groups in `tech.md`
- [ ] `SP1463-T002` Owner: `root-entrypoint` | Dependencies: `SP1463-T001` | Done when: `background.rs` is a thin module entrypoint that declares child modules and re-exports existing public or `pub(super)` surface items | Verify: `cargo check -p harness-server --all-targets`
- [ ] `SP1463-T003` Owner: `dispatch-and-profiles` | Dependencies: `SP1463-T002` | Done when: runtime command dispatch and runtime profile code lives in focused modules with unchanged function names and behavior | Verify: `cargo check -p harness-server --all-targets`
- [ ] `SP1463-T004` Owner: `feedback-and-workers` | Dependencies: `SP1463-T002` | Done when: PR feedback sweeping and runtime worker supervision code lives in focused modules with unchanged function names and behavior | Verify: `cargo check -p harness-server --all-targets`
- [ ] `SP1463-T005` Owner: `recovery-modules` | Dependencies: `SP1463-T002` | Done when: awaiting dependency, PR, system task, checkpoint, and orphan pending recovery code lives in focused modules with unchanged function names and behavior | Verify: `cargo check -p harness-server --all-targets`
- [ ] `SP1463-T006` Owner: `file-size-policy` | Dependencies: `SP1463-T003`, `SP1463-T004`, `SP1463-T005` | Done when: every `http/background*` file is about 1,200 lines or less and `CLAUDE.md` no longer grants the old 3,600-line `background.rs` exemption | Verify: `wc -l crates/harness-server/src/http/background.rs crates/harness-server/src/http/background/*.rs`
- [ ] `SP1463-T007` Owner: `verification` | Dependencies: all implementation tasks | Done when: workspace check, harness-server tests, SpecRail checks, formatting, and clippy all pass | Verify: `cargo check --workspace --all-targets`, `cargo test -p harness-server`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1463`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

The implementation is move-only but touches one production module family. Keep
the root entrypoint and shared helpers serial. If parallel work is used later,
assign disjoint child modules after the re-export shape is established.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-server`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1463`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `wc -l crates/harness-server/src/http/background.rs crates/harness-server/src/http/background/*.rs`

## Handoff Notes

Use `Refs #1463` for the spec PR. The implementation PR should use
`Closes #1463` only after the old U-16 exemption is removed or reduced, the
background files meet the size target, and all verification commands pass.
