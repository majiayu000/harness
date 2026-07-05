# Task Plan

## Linked Issue

GH-1467

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1467-T001` Owner: `config` | Dependencies: none | Done when: `ObserveConfig` exposes `log_retention_max_files` with serde-compatible default `30` and config examples include it | Verify: `cargo test -p harness-core config`
- [ ] `SP1467-T002` Owner: `metadata` | Dependencies: `SP1467-T001` | Done when: runtime log metadata carries both retention days and max-files through CLI/server health surfaces without breaking existing callers | Verify: `cargo check -p harness-server --all-targets`
- [ ] `SP1467-T003` Owner: `cleanup` | Dependencies: `SP1467-T001` | Done when: runtime log startup cleanup prunes by age first, then prunes oldest matching logs beyond the configured max-files cap while ignoring unrelated files | Verify: `cargo test -p harness-cli runtime_log`
- [ ] `SP1467-T004` Owner: `docs` | Dependencies: `SP1467-T001`, `SP1467-T003` | Done when: README and usage guide document runtime log age and count retention, including `0` disabling the count cap | Verify: `rg -n "log_retention_(days|max_files)|runtime log" README.md docs/usage-guide.md`
- [ ] `SP1467-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and clippy pass | Verify: `cargo test -p harness-cli runtime_log`, `cargo test -p harness-core config`, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1467`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

Keep implementation serial. Config, metadata, cleanup, docs, and tests share
small files and should stay in one branch to avoid avoidable conflicts.

## Verification

- `cargo test -p harness-cli runtime_log`
- `cargo test -p harness-core config`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1467`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1467` for the spec PR. The implementation PR should use
`Closes #1467` after count pruning, docs, and verification pass.
