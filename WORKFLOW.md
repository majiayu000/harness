---
# Custom workflow shapes (`definition` block) and prompt-task continuation
# policies are documented in docs/workflow-declarative-definitions.md.
workflow:
  id: github_issue_pr
  version: 1
source:
  kind: github
  repo: majiayu000/harness
base:
  remote: origin
  branch: main
  require_remote_head: true
workspace:
  strategy: worktree
  branch_prefix: harness/
  reuse_existing_workspace: true
  cleanup: on_terminal
hooks:
  timeout_secs: 60
issue_workflow:
  force_execute_label: force-execute
  auto_replan_on_plan_issue: true
pr_scope_guard:
  enabled: true
  max_files_changed: 30
  max_lines_added: 1500
pr_feedback:
  enabled: true
  sweep_interval_secs: 60
  claim_stale_after_secs: 300
  hygiene_enabled: true
  hygiene_interval_secs: 1800
  dirty_age_to_repair_secs: 172800
  dirty_age_to_comment_secs: 604800
  rebase_needed_label: rebase-needed
  hygiene_batch_limit: 25
runtime_dispatch:
  enabled: true
  interval_secs: 30
  batch_limit: 32
  approval_policy: never
  timeout_secs: 3600
  activity_profiles:
    run_local_review:
      timeout_secs: 3600
    inspect_pr_feedback:
      timeout_secs: 3600
runtime_worker:
  enabled: true
  interval_secs: 5
  concurrency: 8
  lease_ttl_secs: 600
runtime_retry_policy:
  max_failed_activity_retries: 6
  retry_delay_secs: 30
  max_retry_delay_secs: 900
  activity_retries: {}
storage:
  schema_namespace: workflow
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
activities:
  implement_issue:
    prompt: default
    validation:
      - cargo fmt --all -- --check
      - cargo check
      - cargo test
      - cargo check --workspace --all-targets
      - cargo clippy --workspace --all-targets -- -D warnings
  run_local_review:
    prompt: pr_feedback
  inspect_pr_feedback:
    prompt: pr_feedback
  quality_gate:
    prompt: quality_gate
---

# Harness Workflow Prompt

You are executing a Harness workflow activity for this repository.

Runtime invariants:

- Work only inside the workspace path supplied by Harness.
- The workspace is admitted by the runtime from the configured remote base before your turn starts.
- Do not switch to or edit the source repository path.
- Treat the workflow database and prompt packet as orchestration state. Do not edit workflow tables directly.

Issue implementation flow:

1. Read the GitHub issue and existing linked PRs when the activity requires implementation.
2. Reproduce or confirm the issue signal before changing code.
3. Make the smallest correct code change.
4. Run the activity validation commands that apply to the touched scope.
5. Run `pr_scope_guard` against the configured base before pushing or creating a PR.
6. If `pr_scope_guard` exceeds `max_files_changed` or `max_lines_added`, do not create a PR; emit `SCOPE_TOO_LARGE` with counts and a decomposition skeleton.
7. Commit, push, and open or update a PR targeting the configured base branch.
8. Include the issue closing line in the PR body when working from an issue.

PR feedback flow:

1. Run the local agent review first after a PR opens or after feedback-driven rework completes.
2. If local review finds blocking issues, address them before querying remote PR feedback again.
3. Only after local review passes, inspect top-level PR comments, inline review comments, review states, checks, and mergeability.
4. Treat actionable remote feedback as blocking until it is fixed or explicitly answered with a justified response.
5. Re-run validation after feedback-driven changes, then return to local review before remote feedback.
6. Before the final response, refresh the PR state from GitHub again, including review threads, review states, checks, mergeability, and the current head commit.
7. If any actionable review thread, requested change, failed check, or mergeability blocker remains, report the activity status as blocked instead of succeeded.

Final response:

- Include changed files, validation commands, and remaining blockers.
- End with a fenced `harness-activity-result` JSON block matching the prompt packet schema.
