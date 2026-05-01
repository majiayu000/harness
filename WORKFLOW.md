---
issue_workflow:
  force_execute_label: force-execute
  auto_replan_on_plan_issue: true
pr_feedback:
  enabled: true
  sweep_interval_secs: 60
  claim_stale_after_secs: 300
runtime_dispatch:
  enabled: true
  interval_secs: 30
  batch_limit: 25
  runtime_kind: codex_jsonrpc
  runtime_profile: codex-default
runtime_worker:
  enabled: true
  interval_secs: 5
  concurrency: 1
  lease_ttl_secs: 900
runtime_retry_policy:
  activity_retries: {}
storage:
  schema_namespace: workflow
---

# Harness Workflow

This file defines high-level workflow policy for Harness.

Current externally configurable rules:

- `issue_workflow.force_execute_label`
  - GitHub issue label that forces execution even when the agent raises a plan concern.

- `issue_workflow.auto_replan_on_plan_issue`
  - Whether `PLAN_ISSUE` should trigger an automatic replan by default.

- `pr_feedback.sweep_interval_secs`
  - Background interval for sweeping issue workflows with attached PRs and enqueuing `pr:N` review/fix tasks.

- `pr_feedback.enabled`
  - Enables or disables automatic background PR feedback sweeping.

- `pr_feedback.claim_stale_after_secs`
  - Maximum age for a `feedback_claimed` placeholder before the sweeper reclaims it
    after an interrupted enqueue path. This does not reclaim live
    `addressing_feedback` tasks with a real `active_task_id`.

- `runtime_dispatch.enabled`
  - Enables the workflow command outbox dispatcher. It is enabled by default
    so accepted workflow commands become runtime jobs.

- `runtime_dispatch.interval_secs`
  - Background interval for converting pending workflow commands into runtime jobs.

- `runtime_dispatch.batch_limit`
  - Maximum command outbox rows dispatched per tick.

- `runtime_dispatch.runtime_kind`
  - Runtime kind for newly created runtime jobs. Supported values are
    `codex_exec`, `codex_jsonrpc`, `claude_code`, `anthropic_api`, and
    `remote_host`.

- `runtime_dispatch.runtime_profile`
  - Runtime profile name stored on newly created runtime jobs.

- `runtime_worker.enabled`
  - Enables the server-owned runtime job worker. It is enabled by default so
    pending runtime jobs are claimed and executed by registered runtime agents.

- `runtime_worker.interval_secs`
  - Background interval for claiming pending runtime jobs.

- `runtime_worker.concurrency`
  - Number of runtime job claims attempted per worker tick.

- `runtime_worker.lease_ttl_secs`
  - Lease duration recorded on claimed runtime jobs.

- `runtime_retry_policy.max_failed_activity_retries`
  - Optional global retry budget for failed workflow runtime activities. When
    unset or zero, failed runtime activities fail the workflow immediately.

- `runtime_retry_policy.activity_retries.<activity>.max_failed_activity_retries`
  - Optional activity-specific retry budget that overrides the global runtime
    activity retry budget for that activity.

- `storage.schema_namespace`
  - Stable namespace used for workflow persistence in Postgres so multiple instances do not split by local `data_dir` path.
