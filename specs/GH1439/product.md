# Product Spec

## Linked Issue

GH-1439

## User Problem

The usage monitor can report `total_tokens = 0` and `request_count = 0` while
workflow-runtime agents are actively burning tokens. Operators see active and
high-burn agent invocations, but the dashboard's primary usage counters remain
empty because workflow-runtime token usage is not persisted into the source the
dashboard reads.

Malformed usage rows are also dropped without a user-visible diagnostic. When
the usage source is missing or corrupt, the dashboard can look clean while its
numbers are incomplete.

## Goals

- Usage monitor totals include token usage emitted by workflow-runtime agent
  turns, including `codex_exec` jobs.
- Malformed usage payloads or corrupt runtime usage rows are surfaced as
  diagnostics and error-level logs instead of disappearing silently.
- Active invocation attribution does not depend on the historical runtime-row
  limit, so running and pending jobs remain visible even when more than 200
  recent jobs exist.
- The existing legacy `llm_usage` event path continues to be counted while
  workflow-runtime usage is migrated to a workflow-runtime-owned source.
- The API response makes source confidence clear when token totals are exact,
  partial, unavailable, or malformed.

## Non-Goals

- Adding billing enforcement, quotas, or automatic shutdown for high burn.
- Replacing local Codex or Claude usage file summaries.
- Changing agent runtime token accounting semantics.
- Removing the legacy task-path `llm_usage` emitter.
- Adding a new dashboard page or authentication surface.

## User-Visible Behavior

`GET /api/usage-monitor` and the `/usage` dashboard show nonzero token and
request totals when workflow-runtime agents emit nonzero `TokenUsage` events
inside the requested window. Totals remain grouped by agent, project, model,
and candidate where attribution exists.

If Harness cannot parse a usage payload, it records a diagnostic count and logs
the malformed source at error level with enough identifying context to debug
the row. The response must not present silently incomplete data as exact.

Running and pending workflow-runtime invocations remain present in
`agent_invocations` and active summaries regardless of the historical `limit`
used for completed jobs.

## Acceptance Criteria

- [ ] Workflow-runtime `TokenUsage` events with nonzero token counts are
      persisted in a workflow-runtime-owned usage source and included in
      `summary.total_tokens`, `summary.request_count`, and grouping arrays.
- [ ] The implementation does not rely on adding new writes to the generic
      observability event store for workflow-runtime usage.
- [ ] Legacy `llm_usage` events still count toward usage totals during the
      migration period.
- [ ] Malformed legacy usage events, malformed workflow-runtime usage rows, and
      corrupt runtime invocation rows are reported through diagnostics and
      error-level logs instead of being silently filtered out.
- [ ] Running and pending runtime jobs are always loaded for active attribution;
      the query `limit` only bounds historical, non-active rows.
- [ ] Process sampling does not run a global sysinfo scan directly on the async
      handler path.
- [ ] Tests cover workflow-runtime usage persistence, usage monitor aggregation,
      malformed-source diagnostics, and active-row limit behavior.

## Edge Cases

- Zero-placeholder token events remain ignored and must not create false
  requests.
- Multiple token updates for the same turn should avoid double-counting
  cumulative totals.
- Missing model, project, or candidate attribution should use the existing
  unknown/unassigned labels rather than dropping the usage.
- If the workflow runtime store is unavailable, the response must clearly mark
  workflow-runtime usage as unavailable while still returning any legacy or
  local-file usage that can be read.
- If a usage row references a runtime job that has since been pruned, the token
  totals should still count while runtime-job attribution falls back to
  available stored fields.

## Rollout Notes

This is an observability correctness fix. It adds or wires a durable
workflow-runtime usage source and changes dashboard diagnostics, but it does
not change agent execution, review gates, or merge authorization.
