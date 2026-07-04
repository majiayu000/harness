# Tech Spec

## Linked Issue

GH-1439

## Product Spec

See `specs/GH1439/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Usage monitor API | `crates/harness-server/src/handlers/usage_monitor.rs` | Aggregates `llm_usage` observability events and active runtime rows. `parse_usage_event` returns `Option`, so malformed events are filtered out without diagnostics. | This is the user-visible surface that reports zero usage. |
| Legacy task usage emitter | `crates/harness-server/src/task_executor/helpers/streaming.rs` | Builds and logs `llm_usage` events for the legacy task path. | This remains a compatibility source but does not cover workflow-runtime hot paths. |
| Workflow-runtime token bridge | `crates/harness-server/src/workflow_runtime_worker/turn_engine/helpers.rs` and `turn_lifecycle.rs` | Handles `StreamItem::TokenUsage` by updating thread state and notifying listeners. It does not persist usage for the usage monitor. | This is the missing runtime usage write path. |
| Workflow runtime store | `crates/harness-workflow/src/runtime/store.rs` and `store_migrations.rs` | Owns runtime jobs, events, commands, and workflow tables. | New workflow-runtime usage persistence belongs here rather than the generic observability event store. |
| Active runtime attribution | `crates/harness-server/src/handlers/usage_monitor.rs` | `load_runtime_usage_rows` applies one `LIMIT` to both active and historical runtime jobs. | More than 200 recent jobs can hide active jobs and inflate external process counts. |
| Process sampling | `crates/harness-server/src/handlers/usage_monitor_process.rs` | Samples system processes through a global sampler lock on the request path. | The API should not block a Tokio worker during a full process scan. |

## Proposed Design

1. Add workflow-runtime usage persistence in `harness-workflow`.
   - Add a migration for a runtime-owned usage table, tentatively
     `runtime_usage_events`.
   - Store enough stable fields to aggregate even after runtime jobs are
     pruned: usage id, runtime job id, command id, workflow id, runtime kind,
     runtime profile, agent label, model, project, optional turn id, optional
     candidate/task attribution keys, token metrics, reported timestamp, and
     updated timestamp.
   - Enforce idempotence for cumulative turn updates with a natural key such
     as `(runtime_job_id, turn_id)` when `turn_id` is available, or another
     deterministic runtime-job scoped key. Upserts should replace cumulative
     values rather than adding duplicates.
2. Persist workflow-runtime `TokenUsage` events from the turn engine.
   - Extend the runtime worker path so token updates have access to the current
     runtime job, workflow command, workflow instance, model, project, and turn
     identifiers needed for usage attribution.
   - Ignore zero-placeholder events using the same semantics as the legacy
     task-path emitter.
   - If persistence fails, log an error and expose a runtime usage persistence
     diagnostic; do not silently continue with an exact-looking zero.
3. Update usage monitor aggregation.
   - Read workflow-runtime usage rows for the requested window from the runtime
     store and combine them with legacy `llm_usage` records.
   - Keep existing grouping behavior for agent, project, model, and candidate
     attribution.
   - Add diagnostics for source availability and malformed/skipped counts,
     including separate counts for legacy event parse failures,
     workflow-runtime usage row parse failures, and runtime invocation row
     parse failures.
   - Set `diagnostics.token_source` or add a source detail field that reflects
     the combined source instead of implying legacy-only `llm_usage_events`.
4. Make active invocation loading independent of historical limits.
   - Load all pending/running runtime jobs for active attribution.
   - Apply `limit` only to completed or otherwise non-active rows inside the
     selected time window.
   - Preserve deterministic ordering for the combined active plus historical
     result set.
5. Move process sampling off the async handler path.
   - Run the sysinfo process snapshot through `tokio::task::spawn_blocking`,
     or an equivalent non-blocking wrapper.
   - If the blocking task fails, return an empty process list with an error
     diagnostic instead of blocking or panicking.

## Data Flow

Workflow-runtime agents emit `AgentEvent::TokenUsage`, which becomes
`StreamItem::TokenUsage` in the turn lifecycle. The turn engine persists the
latest nonzero cumulative usage snapshot to the workflow runtime store with
runtime job, command, workflow, model, project, and attribution metadata.

The usage monitor reads runtime usage rows for the requested window and legacy
`llm_usage` events for compatibility. Parsed records flow into the existing
aggregate helpers. Malformed sources increment diagnostics and emit error-level
logs with source identifiers.

Active runtime invocations are loaded from runtime jobs independently from the
usage rows. Running and pending invocations are always included; historical rows
are bounded by the request limit.

## Alternatives Considered

- Persist workflow-runtime usage by writing new `llm_usage` observability
  events. Rejected because issue evidence notes the generic event store is on a
  contraction path and this would pin new runtime behavior to it.
- Infer token burn from active process count. Rejected because process presence
  cannot provide exact token or request totals.
- Keep silent parse filtering and add a dashboard footnote. Rejected because it
  leaves operators without actionable evidence when numbers are incomplete.
- Remove the runtime-row limit entirely. Rejected because historical rows still
  need a bounded query; only active rows must be unbounded by that limit.

## Risks

- Data correctness: Agent streams may emit cumulative usage updates. The store
  must upsert the latest cumulative snapshot to avoid double-counting.
- Compatibility: Existing legacy task usage and overview usage aggregation must
  continue to work.
- Performance: Usage aggregation must index window and attribution lookups so
  the dashboard remains cheap under many runtime jobs.
- Maintenance: Runtime usage persistence should reuse `UsageMetrics` conversion
  rules to avoid divergent token totals.
- Security: Do not store secrets or full prompts in usage rows.

## Test Plan

- [ ] Unit tests: zero-placeholder usage is skipped; nonzero workflow-runtime
      usage rows upsert by runtime job and turn key.
- [ ] Handler tests: usage monitor totals include workflow-runtime usage and
      legacy `llm_usage` events in the same window.
- [ ] Handler tests: malformed legacy events, malformed runtime usage rows, and
      corrupt invocation rows increment diagnostics and log error-level paths.
- [ ] Runtime worker tests: `StreamItem::TokenUsage` on a workflow-runtime turn
      persists usage with runtime job and workflow metadata.
- [ ] Query tests: pending/running jobs are returned even when historical rows
      exceed the request limit.
- [ ] Async behavior tests or focused unit coverage: process sampling is
      executed through the non-blocking wrapper and reports failure
      diagnostically.

## Rollback Plan

Revert the runtime usage store migration and usage monitor read/write changes.
Existing legacy `llm_usage` aggregation remains available. Runtime usage rows
that were already written can remain inert if the reader is removed.
