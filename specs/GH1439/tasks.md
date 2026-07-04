# Task Plan

## Linked Issue

GH-1439

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1439-T001` Owner: `workflow-runtime-store` | Done when: a workflow-runtime-owned usage table, migration, typed record, query API, and idempotent upsert API persist nonzero runtime token usage without writing new generic `llm_usage` events | Verify: `cargo test -p harness-workflow runtime_usage`
- [ ] `SP1439-T002` Owner: `runtime-worker` | Done when: workflow-runtime `StreamItem::TokenUsage` persists nonzero cumulative usage with runtime job, command, workflow, model, project, turn, and attribution metadata; zero placeholders are skipped | Verify: `cargo test -p harness-server workflow_runtime_worker token_usage`
- [ ] `SP1439-T003` Owner: `usage-monitor` | Done when: `/api/usage-monitor` aggregates workflow-runtime usage rows with legacy `llm_usage` events and reports combined totals by agent, project, model, and candidate | Verify: `cargo test -p harness-server usage_monitor`
- [ ] `SP1439-T004` Owner: `diagnostics` | Done when: malformed legacy usage events, malformed runtime usage rows, corrupt runtime invocation rows, and process sampler failures produce diagnostics plus error-level logs instead of silent filtering | Verify: `cargo test -p harness-server usage_monitor`
- [ ] `SP1439-T005` Owner: `active-attribution` | Done when: running and pending runtime jobs are always included for active attribution while `limit` only bounds historical non-active rows | Verify: `cargo test -p harness-server usage_monitor`
- [ ] `SP1439-T006` Owner: `process-sampling` | Done when: usage monitor process sampling runs through a blocking-safe wrapper and failure is reflected in diagnostics | Verify: `cargo test -p harness-server usage_monitor_process`

## Parallelization

T001 must land before T002 and T003 because both depend on the runtime usage
store API. T002 and T003 can then proceed in parallel if they do not edit the
same files. T004, T005, and T006 touch the usage monitor surface and should be
sequenced after the aggregation shape is clear.

## Verification

- `cargo test -p harness-workflow runtime_usage`
- `cargo test -p harness-server workflow_runtime_worker token_usage`
- `cargo test -p harness-server usage_monitor`
- `cargo test -p harness-server usage_monitor_process`
- `cargo check -p harness-workflow --all-targets`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1439`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Keep the new runtime usage source inside the workflow runtime store. Do not fix
the zero-token dashboard by emitting new generic observability events from the
workflow-runtime hot path. Treat diagnostic counters as part of the acceptance
criteria because the original bug is both missing token ingestion and silent
parse failure.
