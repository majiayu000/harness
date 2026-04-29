---
issue_workflow:
  force_execute_label: force-execute
  auto_replan_on_plan_issue: true
pr_feedback:
  enabled: true
  sweep_interval_secs: 60
  claim_stale_after_secs: 300
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

- `storage.schema_namespace`
  - Stable namespace used for workflow persistence in Postgres so multiple instances do not split by local `data_dir` path.
