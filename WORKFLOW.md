---
issue_workflow:
  force_execute_label: force-execute
  auto_replan_on_plan_issue: true
pr_feedback:
  enabled: true
  sweep_interval_secs: 60
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
