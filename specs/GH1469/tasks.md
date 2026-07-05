# Task Plan

## Linked Issue

GH-1469

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1469-T001` Owner: `config` | Dependencies: none | Done when: `ConcurrencyConfig::default().max_turns` is `Some(20)` and existing explicit request overrides still win | Verify: `cargo test -p harness-core config`
- [ ] `SP1469-T002` Owner: `executor` | Dependencies: `SP1469-T001` | Done when: focused tests show turn budget state crosses at least two execution layers without resetting | Verify: focused `cargo test -p harness-server <budget-test-filter>`
- [ ] `SP1469-T003` Owner: `docs` | Dependencies: `SP1469-T001` | Done when: config example, README, and usage guide document the default and override semantics | Verify: `rg -n "max_turns|turn budget" README.md docs/usage-guide.md config/default.toml.example`
- [ ] `SP1469-T004` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and clippy pass | Verify: `cargo test -p harness-core config`, focused server budget tests, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1469`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

Keep this implementation serial. The config default, docs, and task-executor
tests are small and share budget semantics.

## Verification

- `cargo test -p harness-core config`
- focused `cargo test -p harness-server <budget-test-filter>`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1469`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1469` for the spec PR. The implementation PR should use
`Closes #1469` after the default, docs, and tests merge.
