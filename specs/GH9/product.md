# GH9 Product Spec: GitHub PR Evidence Adapter

GitHub issue: `#9`

## Goals

- Let agents collect GitHub PR merge evidence without manually reshaping `gh`
  output.
- Produce JSON that `checks/pr_gate.py` can evaluate directly.
- Keep GitHub collection separate from SpecRail policy decisions.

## Non-Goals

- Do not merge pull requests.
- Do not resolve review threads.
- Do not write labels, comments, reviews, or branch state.
- Do not make GitHub required for repositories that use offline SpecRail only.

## Users

- Agents preparing a SpecRail merge-readiness report.
- Maintainers who want reproducible evidence behind a merge decision.
- Optional `threads` lanes that need a compact evidence artifact for closure
  audit.

## Behavior

1. The agent runs `python3 checks/github_pr_evidence.py --github-repo OWNER/REPO --pr <number>`.
2. The adapter reads PR metadata with `gh pr view`.
3. The adapter reads thread-aware review data with `gh api graphql`.
4. The adapter prints evidence JSON compatible with `checks/pr_gate.py`.
5. The adapter includes `human_authorization` only when explicitly provided by
   CLI flags.
6. The adapter exits non-zero on malformed GitHub repository names, invalid PR
   numbers, missing `gh`, or malformed command output.

## Acceptance Criteria

- `checks/github_pr_evidence.py` exists and is read-only.
- It converts PR state, draft state, head SHA, merge state, linked issue, checks,
  reviews, and review threads into `pr_gate.py` evidence fields.
- Offline tests cover transformation logic and CLI output without real network.
- Docs and the `specrail-workflow` skill describe the adapter as evidence
  collection, not policy or merge authority.
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH9` passes.
