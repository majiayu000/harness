---
# Central base workflow policy for Harness.
#
# This file lives next to the server config (claude.toml) and is the single
# source of DEFAULT workflow policy for every managed repository. Each repo's
# own `{repo}/WORKFLOW.md` (when present) is deep-merged on top of this base
# field-by-field, so a repo only needs a WORKFLOW.md to override repo-specific
# fields such as `source.repo` or per-language `activities.*.validation`
# commands. A repo with no WORKFLOW.md inherits this base entirely.
workflow:
  id: github_issue_pr
  version: 1
source:
  kind: github
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
repo_backlog:
  # Disabled by default: issues/PRs are picked up via webhook events only, so an
  # idle system (no GitHub events) runs zero LLM turns. Set enabled: true in a
  # repo's own WORKFLOW.md to opt that repo into autonomous LLM backlog polling.
  enabled: false
  poll_interval_secs: 60
  batch_limit: 128
runtime_dispatch:
  enabled: true
  interval_secs: 30
  batch_limit: 32
  approval_policy: never
  timeout_secs: 3600
  activity_profiles:
    inspect_pr_feedback:
      timeout_secs: 3600
    plan_repo_sprint:
      reasoning_effort: xhigh
    poll_repo_backlog:
      runtime_kind: codex_exec
      runtime_profile: codex-backlog-exec
      reasoning_effort: xhigh
      timeout_secs: 600
runtime_worker:
  enabled: true
  interval_secs: 5
  concurrency: 32
  lease_ttl_secs: 600
runtime_retry_policy:
  max_failed_activity_retries: 6
  retry_delay_secs: 30
  max_retry_delay_secs: 900
  activity_retries: {}
storage:
  schema_namespace: workflow
activities:
  implement_issue:
    prompt: default
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

1. Inspect top-level PR comments, inline review comments, review states, checks, and mergeability.
2. Treat actionable feedback as blocking until it is fixed or explicitly answered with a justified response.
3. Re-run validation after feedback-driven changes.
4. Before the final response, refresh the PR state from GitHub again, including review threads, review states, checks, mergeability, and the current head commit.
5. If any actionable review thread, requested change, failed check, or mergeability blocker remains, report the activity status as blocked instead of succeeded.

Final response:

- Include changed files, validation commands, and remaining blockers.
- End with a fenced `harness-activity-result` JSON block matching the prompt packet schema.
