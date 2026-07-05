# Task Plan

## Linked Issue

GH-1472

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1472-T001` Owner: `schema-index` | Dependencies: none | Done when: artifact-by-task query/index coverage is protected without adding a duplicate unscoped index | Verify: `cargo test -p harness-server task_db::migrations`
- [ ] `SP1472-T002` Owner: `config` | Dependencies: none | Done when: workflow storage config includes disabled-by-default task retention fields, defaults, parsing tests, and examples/docs | Verify: `cargo test -p harness-core config::workflow` and `rg -n "task_retention" WORKFLOW.md config/WORKFLOW.md docs/usage-guide.md config/default.toml.example`
- [ ] `SP1472-T003` Owner: `task-db` | Dependencies: `SP1472-T001`, `SP1472-T002` | Done when: `TaskDb` can prune terminal tasks older than a cutoff in bounded batches, deletes artifacts/prompts/checkpoints before parent tasks, and returns structured counts | Verify: `cargo test -p harness-server --test task_db_retention`
- [ ] `SP1472-T004` Owner: `server-loop` | Dependencies: `SP1472-T003` | Done when: an HTTP startup background loop reloads workflow config, respects `storage.task_retention_enabled`, computes cutoffs, calls the task retention DB method, and logs non-empty summaries | Verify: focused server retention tests or `cargo check -p harness-server --all-targets`
- [ ] `SP1472-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and clippy pass | Verify: focused tests above, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1472`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

Keep implementation mostly serial. Config/docs can be drafted independently
from the DB method, but the server loop depends on both the config fields and
the pruning API. Avoid parallel writable lanes across `workflow.rs`,
`task_db`, and `http/mod.rs`.

## Verification

- `cargo test -p harness-server task_db::migrations`
- `cargo test -p harness-core config::workflow`
- `cargo test -p harness-server --test task_db_retention`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1472`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1472` for the spec PR. The implementation PR should use
`Closes #1472` only after the retention config, pruning query, background loop,
docs, tests, and verification land.
