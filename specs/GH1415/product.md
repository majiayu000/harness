# Product Spec

## Linked Issue

GH-1415

## User Problem

Operators who enable auto-merge trust harness to merge a PR only when the deterministic policy gate passes. Today the gate is server-side, but the merge action itself is delegated to an LLM agent that self-reports `merged=true`. A hallucinated or mistaken report marks the workflow as successfully merged while the PR is still open, and the discrepancy is only discovered later (if at all) by periodic reconciliation. Operators cannot currently distinguish "merged because GitHub says so" from "merged because the agent said so".

## Goals

- Terminal merge success is always backed by GitHub-observed state, never by agent output alone.
- Operators can choose whether the merge action is executed by the server or by the agent, per deployment.
- A false agent report is detected at completion time, not at the next reconciliation tick.

## Non-Goals

- Changing the auto-merge policy gate (required checks, approval, draft, review-thread rules stay as-is).
- Supporting merge methods beyond what the repository allows (squash-only repositories stay squash-only).
- Outbound notifications about merge outcomes.

## User-Visible Behavior

- With `merge_execution = "server"`: once the existing gate passes, harness merges the PR through the GitHub API directly. The workflow reaches `done` only after the API confirms the merged state. Agent involvement in the merge step is removed.
- With `merge_execution = "agent"` (default at rollout): behavior looks like today, except that after the agent reports `merged=true`, harness re-reads the PR from the GitHub API before accepting the completion. If GitHub does not show the PR as merged, the activity fails with an explicit validation error instead of completing.
- In both modes, task/workflow detail views show the merge evidence source (API-observed state and PR head SHA at merge time).

## Acceptance Criteria

- [ ] A workflow can reach terminal merge success only when the server has observed `merged=true` / `state=MERGED` from the GitHub API.
- [ ] With a simulated agent that falsely reports `merged=true`, the activity fails validation and the workflow does not reach terminal success (test-covered).
- [ ] `merge_execution` config selects `agent` or `server` mode; default is `agent` (verify-only) for a reversible rollout.
- [ ] Documentation states the token permissions required for server-executed merges.

## Edge Cases

- PR was merged manually by a human between gate pass and merge execution: both modes must treat "already merged" as success, not as an error.
- PR became unmergeable (new push, branch protection change) between gate pass and execution: server mode surfaces the GitHub error and routes the workflow through the existing failure/retry path; it must not retry the merge blindly.
- GitHub API transiently unavailable during verification: completion is deferred/retried per the existing retryable-error classification, not accepted on trust.
- Token lacks merge permission in server mode: fail with an actionable configuration error at merge time; log at error level.

## Rollout Notes

- Ship verify-on-completion first (agent mode); it hardens all existing deployments without changing who executes the merge.
- Server-execution mode is opt-in per deployment via config; repositories opt into auto-merge exactly as today.
- No data migration. Existing in-flight workflows complete under the mode active at completion time.
