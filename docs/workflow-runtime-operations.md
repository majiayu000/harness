# Workflow Runtime Operations

This guide covers manual recovery for stopped GitHub issue workflows and the
watchdog/retention sweepers used to operate the workflow runtime safely.

## Manual Recovery

Use the recovery API after an operator resolves the external condition that
left a workflow `blocked` or `failed`. Recovery is manual by default. Harness
does not periodically retry stopped workflows, and these routes do not change
GitHub labels, comments, or issue state.

### Authentication

The recovery routes use the same API authentication as other non-public HTTP
routes. Configure `[server].api_token` or `HARNESS_API_TOKEN`, then send the
token as a bearer credential. Tokenless access is available only when the
server was deliberately started with `allow_unauthenticated = true` for local
development.

### Requests

Both routes accept a JSON object with a non-empty workflow id and a non-empty
operator reason. The reason is written to the workflow audit trail.

```bash
curl -sS -X POST http://127.0.0.1:9800/api/workflows/runtime/unblock \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_id": "github-issue-workflow-id",
    "reason": "maintainer supplied the requested approval"
  }'

curl -sS -X POST http://127.0.0.1:9800/api/workflows/runtime/retry \
  -H "Authorization: Bearer ${HARNESS_API_TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{
    "workflow_id": "github-issue-workflow-id",
    "reason": "the transient backend outage has been repaired"
  }'
```

| Route | Required current state | Additional rules |
|-------|------------------------|------------------|
| `POST /api/workflows/runtime/unblock` | `blocked` | The stopped activity must be recoverable. |
| `POST /api/workflows/runtime/retry` | `failed` | Failures classified as `fatal` or `configuration` are not retryable. |

The first implementation supports only `github_issue_pr` workflow instances.
`cancelled` and active workflows are not supported by either action. A legacy
stopped instance with no structured stop metadata resumes at `implement_issue`;
partial, malformed, or unsupported stop metadata fails closed without mutating
the workflow.

When structured stop metadata is present, Harness replays the original command
and returns the corresponding active state:

| Stopped activity | New state |
|------------------|-----------|
| `implement_issue` | `implementing` |
| `replan_issue` | `replanning` |
| `merge_pr` | `merging` |
| `run_local_review` | `local_review_gate` |
| `sweep_pr_feedback`, `inspect_pr_feedback`, or `start_child_workflow` | `awaiting_feedback` |
| `address_pr_feedback` | `addressing_feedback` |

Recovery enqueues this replay directly. The resumed active state remains
covered by GitHub intake so the next poll cannot enqueue duplicate work beside
the recovery command.

### Responses

A successful request returns HTTP `200`. The `state` value depends on the
replayed activity described above.

```json
{
  "status": "unblocked",
  "execution_path": "workflow_runtime",
  "workflow_id": "github-issue-workflow-id",
  "previous_state": "blocked",
  "state": "implementing"
}
```

Retry responses use `"status": "retried"` and
`"previous_state": "failed"`.

| HTTP status | Meaning |
|-------------|---------|
| `400 Bad Request` | The JSON syntax is malformed, or `workflow_id` or `reason` is blank. |
| `401 Unauthorized` | The bearer token is missing or invalid. |
| `404 Not Found` | The workflow id does not exist. |
| `409 Conflict` | The action does not match the current state, the failure is non-retryable, the workflow definition is unsupported, or the stopped activity cannot be reconstructed safely. |
| `415 Unsupported Media Type` | The request does not declare a JSON content type. |
| `422 Unprocessable Entity` | Required JSON fields are missing or have the wrong type. |
| `500 Internal Server Error` | Recovery failed while committing the transactional audit/state update. |
| `503 Service Unavailable` | The workflow runtime store is unavailable. |

Do not delete or edit workflow runtime rows to recover a stopped workflow.
Direct database edits are unsupported and can break the event, decision,
command, and runtime-job audit chain. Use the authenticated API so Harness can
preserve stop evidence, supersede stale work safely, record the operator reason,
and enqueue the correct follow-up command atomically.

## Watchdog And Retention Sweepers

Workflow-runtime watchdog and retention sweepers are configured in
`WORKFLOW.md` under `storage`. Both are disabled by default on first rollout:

```yaml
storage:
  orphan_reaper_enabled: true
  orphan_reaper_interval_secs: 3600
  orphan_reaper_legacy_enabled: true
  orphan_reaper_legacy_batch: 200
  workflow_watchdog_enabled: false
  workflow_watchdog_age_minutes: 240
  workflow_watchdog_interval_secs: 300
  workflow_watchdog_batch_size: 100
  runtime_retention_enabled: false
  runtime_retention_days: 30
  runtime_retention_batch_size: 1000
  runtime_retention_interval_secs: 3600
  task_retention_enabled: false
  task_retention_days: 30
  task_retention_batch_size: 1000
  task_retention_interval_secs: 3600
```

The orphan schema reaper is enabled by default. It drops registered
path-derived schemas with dead owner paths and, in bounded batches, legacy
unregistered `h<16-hex>` schemas that cannot be matched to a live workspace
directory or known store path under the configured workspace root.

Enable `workflow_watchdog_enabled` first. It is read-only: aged `blocked` and
`awaiting_feedback` workflow instances appear in `/api/operator-monitor` under
`stuck_workflows` and are logged at error level.

The same operator-monitor response always includes the bounded
`driverless_progress` diagnostic, even when the watchdog is disabled. Each row
reports its workflow, definition, state, age, and state-entry provenance without
changing workflow state or history. Enabling the watchdog also emits these rows
at error level; recovery remains an explicit operator action.

Enable `runtime_retention_enabled` only after the stuck list is clean. Retention
deletes terminal workflow families older than `runtime_retention_days` in
bounded batches and relies on the Postgres runtime-store cascade constraints to
remove events, decisions, commands, jobs, runtime events, and artifacts. Active
workflow families are never pruned.

Enable `task_retention_enabled` only when historical task rows can be deleted.
Retention deletes terminal tasks older than `task_retention_days` in bounded
batches, including task-owned artifacts, prompts, and checkpoints. Active,
pending, dependency-blocked, and resumable tasks are never pruned.
