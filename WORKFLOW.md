---
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
  reuse_existing_workspace: false
  cleanup: on_terminal
hooks:
  timeout_secs: 60
issue_workflow:
  force_execute_label: force-execute
  auto_replan_on_plan_issue: true
pr_feedback:
  enabled: true
  sweep_interval_secs: 60
  claim_stale_after_secs: 300
repo_backlog:
  enabled: true
  poll_interval_secs: 60
  batch_limit: 128
runtime_dispatch:
  enabled: true
  interval_secs: 30
  batch_limit: 25
  runtime_kind: codex_jsonrpc
  runtime_profile: codex-default
  approval_policy: never
runtime_worker:
  enabled: true
  interval_secs: 5
  concurrency: 1
  lease_ttl_secs: 900
runtime_retry_policy:
  activity_retries: {}
storage:
  schema_namespace: workflow
activities:
  implement_issue:
    prompt: default
    validation:
      - cargo fmt --all -- --check
      - cargo check
      - cargo test
      - RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets
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
5. Commit, push, and open or update a PR targeting the configured base branch.
6. Include the issue closing line in the PR body when working from an issue.

PR feedback flow:

1. Inspect top-level PR comments, inline review comments, review states, checks, and mergeability.
2. Treat actionable feedback as blocking until it is fixed or explicitly answered with a justified response.
3. Re-run validation after feedback-driven changes.

Final response:

- Include changed files, validation commands, and remaining blockers.
- End with a fenced `harness-activity-result` JSON block matching the prompt packet schema.
