# GH7 Product Spec: Deterministic PR Merge Gate

GitHub issue: `#7`

## Goals

- Give agents a deterministic, offline merge-readiness check before a maintainer
  merges a pull request.
- Convert the manual closure audit proven by the `rclean` and `litellm-rs`
  pilots into a reusable SpecRail gate.
- Keep SpecRail advisory: agents may report merge readiness, but humans still
  own final approval and merge.

## Non-Goals

- Do not add a GitHub API adapter in this change.
- Do not add automatic merge, final approval, label writes, or thread writes.
- Do not make `threads` mandatory; it remains an optional orchestration layer.

## Users

- Agents preparing a PR handoff or merge-readiness report.
- Maintainers who want a compact list of missing merge evidence.
- Repositories adopting SpecRail without a hosted bot.

## Behavior

1. The agent records PR evidence in a local JSON file.
2. The agent runs `python3 checks/pr_gate.py --repo . --evidence <path>`.
3. The gate reports `allowed` only when all deterministic merge evidence is
   present and clean.
4. The gate reports `needs_human` when deterministic checks pass but explicit
   human merge authorization is missing.
5. The gate reports `blocked` when CI is pending/failing, review threads are
   unresolved, changes are requested, merge state is unsafe, the PR is draft, or
   the linked issue/head SHA is missing.

## Acceptance Criteria

- `checks/pr_gate.py` exists and is read-only.
- The gate validates PR state, linked issue, head SHA, CI, review threads,
  review decisions, merge state, and human authorization.
- The PR template and agent docs name the merge gate and required evidence.
- Tests cover allowed, missing authorization, pending CI, and unresolved thread
  cases.
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH7` passes.
