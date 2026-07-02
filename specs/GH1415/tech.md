# Tech Spec

## Linked Issue

GH-1415

## Product Spec

See `specs/GH1415/product.md`.

## Current System

- Gate: `request_auto_merge_if_enabled` (`crates/harness-server/src/http/background.rs:963`) evaluates `auto_merge_snapshot_satisfies_policy` (`crates/harness-server/src/http/auto_merge.rs:82`) and, on pass, enqueues a `merge_pr` command.
- Execution: the `merge_pr` activity is rendered into an agent prompt packet (`crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs:544-552`) whose success contract is the agent re-reading GitHub and reporting `state=merged or merged=true`.
- Completion: the reducer accepts the structured activity result; no server-side confirmation read exists. Reconciliation later maps an observed `PrMerged` to `done` (`crates/harness-server/src/reconciliation.rs:388,401`).
- Existing infrastructure to reuse: reqwest-based GitHub GraphQL snapshot fetcher `fetch_github_pr_snapshot` (`crates/harness-server/src/github_pr_snapshot.rs:135`, GraphQL URL at `:12`) and token resolution `resolve_github_token` (`crates/harness-server/src/github_auth.rs:1`).
- Architecture constraint: harness crates must not spawn `gh`/`git` subprocesses; direct HTTPS API calls are the only permitted server-side GitHub interaction.

## Proposed Design

Two phases behind one config enum.

Phase 1 — verify-on-completion (mode `agent`, new default behavior):

1. Add a completion interceptor for `("github_issue_pr", "merge_pr")` activity results in the workflow runtime worker completion path (alongside the existing activity-result validation in `workflow_runtime_worker/activity_result.rs`).
2. When the agent reports merge success, call `fetch_github_pr_snapshot` for the bound PR. Accept completion only if the snapshot shows the PR merged; otherwise convert the result into a validation failure (`invalid_agent_output` class) so the existing reducer failure/retry path applies.
3. Record the verification evidence (merged flag, merge commit SHA / PR state, snapshot timestamp) into the activity result artifacts so the decision record is auditable.

Phase 2 — server-executed merge (mode `server`, opt-in):

1. Add `merge_pull_request` to a small REST client module next to `github_pr_snapshot.rs` (e.g. `github_pr_merge.rs`): `PUT https://api.github.com/repos/{owner}/{repo}/pulls/{number}/merge` with `merge_method` resolved from repository settings (squash for this project), using the same reqwest client pattern and `resolve_github_token`.
2. In the dispatcher, when mode is `server`, route the `merge_pr` command to a builtin lifecycle activity (same mechanism as `mark_bound_issue_done`) instead of an agent prompt packet. The builtin: optional freshness re-check of the gate snapshot -> merge call -> confirmation read via `fetch_github_pr_snapshot` -> structured activity result.
3. "Already merged" (HTTP 405/409 with merged state, or snapshot shows merged) is success. Other 4xx/5xx map to the existing retryable/non-retryable error classification (`reducer/runtime_failure.rs`).

Config: `merge_execution: "agent" | "server"` under the existing auto-merge/intake config (`crates/harness-server/src/config/intake.rs`), default `agent`. Startup logs the active mode.

## Data Flow

- Inputs: workflow instance data (repo slug, PR number, expected head SHA), GitHub token from config/env.
- External calls: GitHub GraphQL (existing snapshot query) for verification; GitHub REST merge endpoint (new) in server mode.
- Outputs: structured activity result with verification evidence; decision record via the existing reducer transaction; no new tables.
- Persistence: evidence stored in existing activity-result/decision artifacts; config persisted in server config file.

## Alternatives Considered

- Keep agent-executed merge and rely on faster reconciliation only: still leaves a window where the workflow is terminally "done" while the PR is open; rejected because terminal state should never be evidence-free.
- GraphQL `mergePullRequest` mutation instead of REST: viable; REST chosen because the merge-method semantics and error surface are simpler and better documented. Revisit if the client consolidates on GraphQL.
- Removing agent mode entirely in one step: rejected; verify-first rollout keeps behavior reversible and lets existing deployments harden without new token permissions.

## Risks

- Security: server mode requires a token with `contents: write` / merge permission; documented, and mode is opt-in. Verify-only mode needs no new permissions.
- Compatibility: agents that previously "completed" merges dishonestly will now fail validation; this is the intended behavior change and is confined to the merge activity.
- Performance: one extra GraphQL read per merge completion; negligible against agent runtime.
- Maintenance: one new small REST module; reuses existing token, client, and error-classification machinery.

## Test Plan

- [ ] Unit tests: verification interceptor accepts merged snapshot, rejects open/closed-unmerged snapshot, defers on transient fetch error; merge client maps 200/405/409/422 correctly (mocked HTTP).
- [ ] Integration tests: reducer path — false `merged=true` report produces a blocked/failed decision, not terminal success; server-mode builtin produces terminal success only after confirmation read (mock GitHub server).
- [ ] Manual verification: on a test repository, run one auto-merge in each mode; confirm decision records contain API-observed evidence.

## Rollback Plan

`merge_execution = "agent"` plus a `verify_merge_completion = false` escape hatch restores today's exact behavior without code rollback. Both flags read at dispatch time, so no migration is needed to revert.
