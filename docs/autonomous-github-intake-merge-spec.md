# Autonomous GitHub Intake And Merge Closure Spec

Status: Draft v1
Author: harness contributor
Audience: harness maintainers and implementers

## Normative Language

`MUST`, `MUST NOT`, `SHOULD`, and `MAY` follow RFC 2119.

## 1. Problem

Harness can currently reach `ready_to_merge`, but an open GitHub issue can still
cause repeated agent work because discovery and feedback checks are agent-owned.
That is the wrong ownership boundary.

The current runtime has three expensive loops:

1. `repo_backlog` periodically enqueues `poll_repo_backlog`.
2. `poll_repo_backlog` starts an agent to call GitHub and decide whether issues
   are already covered.
3. PR feedback inspection can start an agent even when GitHub facts are already
   enough to prove that nothing changed.

This means an issue that is open only because its PR is waiting at
`ready_to_merge` can still burn model tokens. The server already has enough
cheap, structured information to avoid that. GitHub issue state, PR head SHA,
status checks, mergeability, review states, and review thread resolution are
machine facts; they MUST be collected by the server before any agent is allowed
to run.

## 2. Non-Compatibility Decision

This design is intentionally not backward compatible with the current
`repo_backlog` workflow.

The implementation MUST delete the legacy agent-driven repository backlog
workflow and its reducer path:

- no `repo_backlog` workflow instances;
- no `poll_repo_backlog` runtime activity;
- no `plan_repo_sprint` runtime activity;
- no repo-level `idle -> scanning -> planning_batch -> dispatching` state
  machine;
- no agent call whose only purpose is to list open GitHub issues;
- no agent call whose only purpose is to prove a covered issue is covered.

Existing `repo_backlog` rows MAY be deleted during migration. The replacement
runtime path is allowed to break old `repo_backlog` state because that old state
is the source of the token-burning behavior.

This is not a greenfield rewrite. The implementation MUST consolidate existing
intake, issue-lifecycle, PR-fact, and reconciliation code. It MUST NOT create a
parallel GitHub orchestration lane beside the current `IssueLifecycleState`,
`IssueWorkflowStore`, `GitHubIssuesPoller`, `IntakeOrchestrator`, review-loop PR
fact query, or reconciliation modules.

## 3. Goals

- Make GitHub intake server-owned and idempotent.
- Make "covered issue" detection a cheap database and GitHub API check.
- Keep agents only for tasks that need repository or GitHub write work:
  implementation, feedback repair, optional dependency analysis, and merge.
- Prevent `ready_to_merge` workflows from dispatching more agents unless the PR
  head, checks, review state, or merge policy changes.
- Support a full autonomous closure path:
  issue discovered -> implementation PR -> PR facts clean -> merge -> done.
- Record enough remote facts to explain why the system did or did not dispatch
  an agent.
- Fail closed on unsafe merge conditions.

## 4. Non-Goals

- No generic task queue redesign.
- No support for multiple legacy GitHub intake implementations.
- No automatic merge when the repository has not explicitly opted into merge
  automation.
- No server-side `git` or `gh` process spawning. Harness server may use GitHub
  APIs for read-only and low-risk orchestration facts. Repository edits and
  merge execution remain agent activities unless a future architecture decision
  explicitly permits server-side GitHub writes.

## 5. Ownership Model

### Server-Owned Facts

The server owns cheap remote facts:

- open issues;
- issue labels;
- issue closed state;
- PR number and URL;
- PR head SHA;
- PR base branch;
- status check rollup;
- mergeability;
- review decision;
- unresolved, non-outdated review threads;
- PR merged state;
- issue-to-PR closing references.

Server-owned facts MUST NOT require agent tokens.

### Agent-Owned Work

Agents own work that requires judgment plus repository or GitHub write action:

- `implement_issue`;
- `address_pr_feedback`;
- `merge_pr`;
- optional `analyze_dependencies` for issue sets that contain explicit
  dependency markers.

An agent MUST NOT be dispatched unless a server-side gate records why the work is
needed.

## 6. Brownfield Integration Architecture

```text
GitHub Webhook/Poller
        |
        v
GitHubFactCollector
  from intake/github_issues.rs + review_loop.rs
        |
        v
remote_fact_snapshots
        |
        v
CoverageGate
  wraps IssueWorkflowStore::get_by_issue
        |
        +------ covered or unchanged facts -----> stop, no agent
        |
        v
IssueWorkflowWriter
  wraps workflow_runtime_submission + IssueWorkflowStore
        |
        v
existing github_issue_pr / IssueLifecycleState workflow
        |
        v
Runtime Dispatcher  ----->  implement_issue agent
        |
        v
PRFactCollector
  extracted from task_executor/review_loop.rs
        |
        v
PRStateGate
        |                    |
        |                    +-- clean + auto merge disabled -> ready_to_merge, no agent
        |                    +-- clean + auto merge enabled  -> merge_pr agent
        |                    +-- feedback/check failure      -> address_pr_feedback agent
        v
MergeReconciler  ----->  done
```

The poller and webhook path feed the same deterministic fact collector. Webhooks
provide low-latency updates; polling is a recovery path and a source of truth
refresh.

## 6.1 Existing Assets And Disposition

| Existing asset | Current responsibility | Required disposition |
|---|---|---|
| `crates/harness-workflow/src/issue_lifecycle.rs` | Durable issue lifecycle with `Discovered`, `Scheduled`, `Implementing`, `PrOpen`, `AwaitingFeedback`, `FeedbackClaimed`, `AddressingFeedback`, `ReadyToMerge`, `Blocked`, `Done`, `Failed`, `Cancelled`. | Extend with `AwaitingDependencies` and `Merging`. Preserve `FeedbackClaimed`; do not create a second issue lifecycle enum. |
| `IssueWorkflowStore::get_by_issue(project_id, repo, issue_number)` | Existing issue coverage lookup. | Become the coverage gate's authoritative lookup. Broaden the covered-state policy; do not replace it with a new lookup system. |
| `crates/harness-server/src/intake/github_issues.rs` | Existing GitHub issue REST poller and label filtering. | Extract or wrap as the issue portion of `GitHubFactCollector`. Remove `mark_dispatched` from the autonomous runtime path because workflow coverage becomes the source of dedupe truth. |
| `crates/harness-server/src/intake/mod.rs` | Large legacy orchestrator with sprint planning, DAG slot filling, source dedupe, and task enqueue. | Split. Keep reusable `IncomingIssue`, source plumbing, dependency/DAG helpers if still needed. Replace `run_repo_sprint` and task-row enqueue with direct workflow creation through the runtime submission path. |
| `crates/harness-server/src/task_executor/review_loop.rs` | Existing PR REST/GraphQL facts, review signals, status rollup, review threads, external merged/closed classification. | Extract PR fact structs and queries into a shared module used by both legacy review loop and new PR readiness gate during transition. Do not implement a second PR fact collector. |
| `crates/harness-server/src/reconciliation.rs` | Existing external PR merged/closed reconciliation and workflow completion updates. | Reuse for merge confirmation and `done` transition; add fact-hash awareness if needed. |
| `crates/harness-server/src/workflow_runtime_pr_feedback.rs` | Existing PR feedback workflow updates parent issue workflows to `ready_to_merge` and records merged PRs. | Keep reducer semantics, but drive inspection from server-side PR fact changes instead of periodic agent sweeps when facts are unchanged. |
| `crates/harness-workflow/src/runtime/repo_backlog.rs`, `workflow_runtime_repo_backlog.rs`, `repo_backlog_candidates.rs`, `repo_backlog_completion.rs` | Legacy agent-driven repo backlog state machine. | Delete. Any still-useful constants or helpers must be moved and renamed; no `repo_backlog` runtime definition remains. |

## 6.2 Sprint Planner Disposition

The current sprint planner agent inside `IntakeOrchestrator::run_repo_sprint`
does three different jobs:

1. determine which open issues exist;
2. dedupe issues against already-running work;
3. group issues into dependency-aware execution rounds.

Jobs 1 and 2 become server-owned and MUST NOT dispatch agents. Job 3 is kept
only as optional dependency analysis. The implementation MUST either:

- reuse the existing DAG parsing and `ready_issues` helpers from
  `intake/mod.rs`; or
- move those helpers into a smaller dependency-analysis module.

The legacy sprint planner MUST NOT remain as an always-on agent step. It may be
replaced by a batched `analyze_dependencies` activity only when issue title/body
contains explicit dependency markers.

## 7. Data Model

### `remote_fact_snapshots`

New table in the workflow runtime schema.

```sql
CREATE TABLE workflow_runtime.remote_fact_snapshots (
  id UUID PRIMARY KEY,
  provider TEXT NOT NULL,
  repo TEXT NOT NULL,
  subject_type TEXT NOT NULL,
  subject_number BIGINT NOT NULL,
  subject_url TEXT,
  head_sha TEXT,
  state TEXT NOT NULL,
  fact_hash TEXT NOT NULL,
  facts JSONB NOT NULL,
  fetched_at TIMESTAMPTZ NOT NULL,
  UNIQUE (provider, repo, subject_type, subject_number)
);
```

`fact_hash` is the single authoritative dedupe primitive. It MUST be stable for
semantically identical facts.

Existing command dedupe keys MAY still exist because `WorkflowCommand` already
uses them, but they MUST be derived from `fact_hash` plus activity identity. They
are not an independent source of truth. The autonomous runtime path MUST NOT use
`GitHubIssuesPoller::mark_dispatched` as durable dedupe state.

### `github_issue_pr` Workflow Data

The workflow instance data MUST include:

```json
{
  "repo": "owner/repo",
  "issue_number": 171,
  "issue_url": "https://github.com/owner/repo/issues/171",
  "labels": ["harness-auto-test"],
  "pr_number": 172,
  "pr_url": "https://github.com/owner/repo/pull/172",
  "pr_head_sha": "3b0d417...",
  "last_remote_fact_hash": "sha256:...",
  "merge_policy": "manual | auto | disabled",
  "merge_method": "squash | merge | rebase",
  "merge_attempted_head_sha": "3b0d417..."
}
```

`last_remote_fact_hash` links the workflow projection to
`remote_fact_snapshots`. Repeated agent work is prevented by checking whether a
command for `(workflow_id, activity, fact_hash)` already exists or has already
reached a terminal result.

## 8. Workflow States

The existing `IssueLifecycleState` is the GitHub issue lifecycle. The
implementation MUST extend it instead of creating a parallel state graph.

Allowed states:

- `discovered`
- `awaiting_dependencies`
- `scheduled`
- `implementing`
- `pr_open`
- `awaiting_feedback`
- `feedback_claimed`
- `addressing_feedback`
- `ready_to_merge`
- `merging`
- `done`
- `blocked`
- `failed`
- `cancelled`

Required transitions:

| From | Server fact or activity result | To | Agent allowed |
|---|---|---|---|
| none | uncovered open issue | `discovered` | no |
| `discovered` | dependency marker found | `awaiting_dependencies` | optional `analyze_dependencies` |
| `discovered`, `awaiting_dependencies`, `scheduled` | implementation ready | `implementing` | `implement_issue` |
| `implementing` | PR artifact produced | `pr_open` | no |
| `implementing` | issue confirmed closed | `done` | no |
| `implementing` | no PR or closure proof | `blocked` | no |
| `pr_open`, `awaiting_feedback` | remote facts changed | server evaluates | no |
| `awaiting_feedback` | feedback claim exists | `feedback_claimed` | no |
| `feedback_claimed` | claim requires repair | `addressing_feedback` | `address_pr_feedback` |
| `feedback_claimed` | claim has no actionable feedback | `awaiting_feedback` or `ready_to_merge` | no |
| `pr_open`, `awaiting_feedback` | unresolved actionable feedback or failing checks | `addressing_feedback` | `address_pr_feedback` |
| `addressing_feedback` | repair pushed or replied | `awaiting_feedback` | no |
| `pr_open`, `awaiting_feedback`, `addressing_feedback` | checks green, mergeable, no unresolved actionable feedback | `ready_to_merge` | no |
| `ready_to_merge` | merge automation disabled | `ready_to_merge` | no |
| `ready_to_merge` | auto merge enabled and merge gate passes | `merging` | `merge_pr` |
| `merging` | PR merged | `done` | no |
| any non-terminal | operator cancel | `cancelled` | no |

`ready_to_merge` is a quiescent state. It MUST NOT enqueue more agent work unless
one of these facts changes:

- PR head SHA;
- status check rollup;
- review threads;
- review decision;
- mergeability;
- merge policy;
- issue or PR closed/merged state.

## 9. GitHub Fact Collection

### Issue Polling

The issue poller MUST be extracted from or wrap the existing
`intake::github_issues::GitHubIssuesPoller`; it MUST NOT duplicate the GitHub
REST parser. The server polls:

```text
GET /repos/{owner}/{repo}/issues?state=open&per_page=100
```

Rules:

- filter out entries with `pull_request != null`;
- apply configured labels;
- paginate until exhaustion;
- store a `remote_fact_snapshots` row per issue;
- never dispatch an agent during this phase.

### PR Facts

The PR fact collector MUST be extracted from the existing review-loop PR query
code (`task_executor/review_loop.rs`) and made reusable. The server uses GraphQL
for PR readiness facts:

```graphql
pullRequest(number: $number) {
  number
  state
  isDraft
  mergeStateStatus
  reviewDecision
  headRefOid
  baseRefName
  closingIssuesReferences(first: 20) { nodes { number url } }
  statusCheckRollup { state contexts(first: 100) { nodes { ... } } }
  reviewThreads(first: 100) {
    nodes {
      isResolved
      isOutdated
      comments(first: 20) { nodes { author { login } body createdAt url } }
    }
  }
}
```

The server MUST treat unresolved, non-outdated review threads as actionable
feedback unless explicitly configured otherwise.

## 10. Coverage Gate

An open issue is covered when a `github_issue_pr` workflow exists for
`(project_id, repo, issue_number)` in any of these states:

- `discovered`
- `awaiting_dependencies`
- `scheduled`
- `implementing`
- `pr_open`
- `awaiting_feedback`
- `feedback_claimed`
- `addressing_feedback`
- `ready_to_merge`
- `merging`
- `done`
- `blocked`
- `failed`
- `cancelled`

Terminal workflows still count as covered. The system MUST NOT automatically
create a replacement workflow for a failed or cancelled issue. Retry requires an
explicit operator action that creates a new run or resets state.

This rule is the direct token-burn fix: an open issue with a `ready_to_merge`
workflow is covered, so issue polling stops before any agent dispatch.

## 11. Agent Dispatch Gate

Every agent activity MUST pass an `AgentDispatchGate`:

```json
{
  "activity": "implement_issue",
  "workflow_id": "...",
  "repo": "owner/repo",
  "issue_number": 171,
  "reason": "uncovered_issue_ready_for_implementation",
  "fact_hash": "sha256:..."
}
```

If the same workflow already recorded the same `activity + fact_hash`, the
dispatcher MUST skip the agent call. Any `WorkflowCommand` dedupe key is a
derived implementation detail and MUST be computed from the same `fact_hash`.

Allowed reasons:

- `uncovered_issue_ready_for_implementation`;
- `dependency_analysis_required`;
- `actionable_pr_feedback`;
- `checks_failed`;
- `auto_merge_gate_passed`.

No other reason may dispatch an agent.

## 12. PR Readiness Gate

The server computes:

```rust
enum PrReadiness {
    NeedsFeedbackRepair { evidence: RemotePrEvidence },
    NeedsCiRepair { evidence: RemotePrEvidence },
    WaitingForChecks { evidence: RemotePrEvidence },
    WaitingForMergeability { evidence: RemotePrEvidence },
    ReadyToMerge { evidence: RemotePrEvidence },
    Merged { evidence: RemotePrEvidence },
    ClosedUnmerged { evidence: RemotePrEvidence },
}
```

`ReadyToMerge` requires all of:

- PR is open;
- PR is not draft;
- head SHA is known;
- all required checks are successful;
- no unresolved, non-outdated review threads;
- merge state is `CLEAN` or equivalent accepted state;
- base branch matches workflow expectation.

CI green alone is not enough.

When `ReadyToMerge` is reached, the workflow transitions to `ready_to_merge`
without an agent.

## 13. Automatic Merge

### Configuration

```toml
[intake.github]
enabled = true
poll_interval_secs = 60

[intake.github.auto_merge]
enabled = true
method = "squash"
delete_branch = true
require_review_threads_resolved = true
require_clean_merge_state = true

[[intake.github.repos]]
repo = "majiayu000/rclean"
label = "harness-auto-test"
project_root = "/Users/apple/Desktop/code/AI/tool/rclean"
auto_merge = true
```

Auto merge MUST be explicit at either the global or per-repo level. If omitted,
the system stays at `ready_to_merge` without burning tokens.

### Merge Gate

Before `merge_pr` dispatch, server MUST record a fresh `RemotePrEvidence` object
whose `head_sha` matches the workflow's `pr_head_sha`.

If any merge precondition changes after `ready_to_merge`, the workflow MUST leave
`ready_to_merge` and re-evaluate before merge.

### Merge Execution

`merge_pr` is a runtime activity. The prompt MUST require the agent to:

1. Re-read PR head SHA, status checks, review threads, and merge state.
2. Abort if the head SHA differs from the server-provided head SHA.
3. Merge using the configured method.
4. Return a structured `pull_request` artifact with merged state, merge commit,
   PR number, and PR URL.

The server then re-fetches the PR. Only GitHub-confirmed merged state can move
the workflow to `done`.

`MergeApproved` and existing runtime merge endpoints MAY remain as manual
approval inputs. Auto merge adds a new approval source, not a second completion
model. All successful merge paths MUST converge on the same `PrMerged` /
`WorkflowDone` persistence path that reconciliation already uses.

## 14. Webhook Handling

Webhooks update `remote_fact_snapshots` and trigger the same gates as polling.

Webhook events that MUST be handled:

- `issues.opened`;
- `issues.reopened`;
- `issues.closed`;
- `pull_request.opened`;
- `pull_request.synchronize`;
- `pull_request.closed`;
- `check_suite.completed`;
- `check_run.completed`;
- `pull_request_review.submitted`;
- `pull_request_review_thread.resolved` when available;
- `issue_comment.created`;
- `pull_request_review_comment.created`.

Webhook delivery is an acceleration path only. Polling remains the recovery
path.

## 15. Token Budget Contract

The system MUST expose per-repo token dispatch counters:

- `server_github_poll_count`;
- `agent_implement_issue_count`;
- `agent_address_feedback_count`;
- `agent_merge_pr_count`;
- `agent_dependency_analysis_count`;
- `agent_skipped_covered_issue_count`;
- `agent_skipped_same_fact_hash_count`.

Budget invariants:

- Polling an unchanged repository with only covered issues MUST dispatch zero
  agents.
- A workflow in `ready_to_merge` with unchanged PR facts MUST dispatch zero
  agents.
- A merged PR reconciliation MUST dispatch zero agents.
- Repeated polling of the same open covered issue MUST dispatch zero agents.

## 16. Migration

The cutover migration MUST:

1. Stop the server.
2. Delete or archive all `repo_backlog` workflow rows, commands, decisions,
   events, and runtime jobs.
3. Remove `repo_backlog` workflow definitions.
4. Add `remote_fact_snapshots`.
5. Add any required indexes:

```sql
CREATE INDEX remote_fact_snapshots_repo_subject_idx
  ON workflow_runtime.remote_fact_snapshots
  (repo, subject_type, subject_number);

CREATE INDEX workflow_instances_issue_lookup_idx
  ON workflow_runtime.workflow_instances
  ((data->>'repo'), ((data->>'issue_number')::bigint))
  WHERE definition_id = 'github_issue_pr';
```

6. Start the new binary.
7. Run one poll tick in dry-run mode and compare observed issue coverage before
   enabling workflow creation.

No downgrade path is required.

## 16.1 Deployment Preconditions

Before implementation starts, the operator MUST capture the current production
or dogfood intake mode:

- poll;
- webhook;
- hybrid;
- disabled.

If the active deployment is webhook-only, the implementation still matters, but
the immediate token-burn risk is limited to poll-mode and recovery-mode runs.
The spec therefore targets the runtime architecture, not only the currently
deployed configuration. The acceptance test MUST include a poll-mode test repo
because poll-mode is the path that reproduced the repeated agent dispatch.

## 17. Deleted Code

The implementation MUST remove:

- `crates/harness-workflow/src/runtime/repo_backlog.rs`;
- `crates/harness-workflow/src/runtime/reducer/repo_backlog_candidates.rs`;
- `crates/harness-workflow/src/runtime/reducer/repo_backlog_completion.rs`;
- repo backlog validator entries;
- `crates/harness-server/src/workflow_runtime_repo_backlog.rs`;
- `poll_repo_backlog` prompt packet schemas;
- `plan_repo_sprint` prompt packet schemas;
- stale repo backlog recovery code;
- default runtime profiles for `poll_repo_backlog` and `plan_repo_sprint`;
- config fields whose only consumer was the repo backlog workflow.

The implementation SHOULD keep only shared helpers that are still useful after
renaming and moving them into the new intake modules.

## 17.1 Refactored Code

The implementation MUST refactor, not duplicate, these existing components:

- `crates/harness-server/src/intake/mod.rs`
  - remove or split `run_repo_sprint` from always-on autonomous GitHub intake;
  - keep `IncomingIssue` and reusable dependency/DAG helpers;
  - stop using task-row enqueue as the GitHub autonomous runtime path;
  - stop using `mark_dispatched` as the durable dedupe primitive.
- `crates/harness-server/src/intake/github_issues.rs`
  - keep the REST `/issues` client/parser;
  - expose a fact-collection API that returns issue facts and `fact_hash`;
  - keep legacy `IntakeSource` only if non-runtime callers still need it.
- `crates/harness-server/src/task_executor/review_loop.rs`
  - move PR fact query structs and GitHub GraphQL/REST helpers into a shared
    module;
  - keep review-loop behavior by calling the shared module;
  - do not leave a private PR fact collector in review loop plus a second one in
    runtime intake.
- `crates/harness-server/src/reconciliation.rs`
  - reuse external merged/closed classification for merge confirmation;
  - update only where needed to consume `remote_fact_snapshots`.
- `crates/harness-server/src/workflow_runtime_pr_feedback.rs`
  - keep parent propagation to `ready_to_merge`;
  - add fact-hash checks so unchanged PR facts do not redispatch inspection.

## 18. Target Modules And Source Of Code

### `github_fact_collector.rs`

Source: `intake/github_issues.rs` plus PR fact code extracted from
`task_executor/review_loop.rs`.

Responsibility: collect issue and PR facts, normalize them, and compute
`fact_hash`.

### `github_coverage_gate.rs`

Source: existing `IssueWorkflowStore::get_by_issue` and workflow runtime store
lookups.

Responsibility: decide whether an issue is uncovered, covered, terminal-covered,
or explicitly retryable by operator action.

### `github_intake_controller.rs`

Source: selected orchestration code from `intake/mod.rs`,
`workflow_runtime_submission.rs`, and HTTP background polling.

Responsibility: coordinate polling/webhook events, coverage checks, dependency
analysis, and workflow creation.

### `pr_readiness_gate.rs`

Source: `task_executor/review_loop.rs` PR signal classification and
`workflow_runtime_pr_feedback.rs` outcome semantics.

Responsibility: compute PR readiness from server-owned facts and decide whether
to wait, repair, mark ready, or reconcile merged/closed.

### `merge_controller.rs`

Source: existing runtime merge approval, PR reconciliation, and task mutation
routes.

Responsibility: decide whether `merge_pr` may be enqueued and converge auto and
manual merge paths on the same persisted completion events.

## 19. Tests

Unit tests:

- open issue without workflow creates `github_issue_pr`;
- open issue with `ready_to_merge` workflow dispatches zero agents;
- open issue with `failed` workflow dispatches zero agents;
- repeated identical issue poll dispatches zero agents;
- legacy `mark_dispatched` state does not affect runtime coverage decisions;
- PR with green checks and no unresolved review threads transitions to
  `ready_to_merge` without agent;
- `ready_to_merge` unchanged PR facts dispatch zero agents;
- PR head SHA change leaves `ready_to_merge` and re-evaluates;
- unresolved review thread dispatches exactly one `address_pr_feedback`;
- repeated same unresolved review thread does not redispatch until fact hash
  changes;
- auto merge disabled leaves workflow at `ready_to_merge`;
- auto merge enabled dispatches exactly one `merge_pr`;
- merge result moves workflow to `done` only after GitHub confirms merged.

Integration tests:

- mock GitHub issue list with one open issue, poll twice, assert one workflow and
  zero second-pass agent jobs;
- mock ready PR with all checks success and no review threads, assert
  `ready_to_merge`;
- mock merged PR, assert `done` and issue closure evidence;
- mock check failure, assert `address_pr_feedback` dispatch once;
- mock unresolved review thread, assert feedback dispatch once;
- restart server after snapshots exist, poll again, assert no duplicate workflows.
- review-loop PR fact query and PR readiness gate use the same shared fact
  collector module; tests fail if a private duplicate collector is kept.
- `IntakeOrchestrator::run_repo_sprint` is not invoked by autonomous GitHub
  runtime intake.

Live smoke:

1. Configure a single test repo and label.
2. Create one labeled issue.
3. Wait for `github_issue_pr` creation.
4. Confirm `implement_issue` runs as `codex_exec`.
5. Confirm PR is created.
6. Confirm server-side PR readiness reaches `ready_to_merge`.
7. With auto merge enabled, confirm `merge_pr` runs once.
8. Confirm PR merged, issue closed, workflow `done`.
9. Wait two more poll intervals and confirm no additional agent jobs for the
   closed or covered issue.

## 20. Acceptance Criteria

The design is implemented when:

- `repo_backlog` no longer exists as a workflow definition.
- There is no `poll_repo_backlog` or `plan_repo_sprint` activity.
- GitHub issue polling uses server-owned API calls only.
- Covered open issues, including `ready_to_merge`, dispatch zero agents.
- PR readiness is computed from server-owned facts.
- Existing `review_loop.rs` PR fact collection is extracted or wrapped; no second
  PR fact collector exists.
- `intake/mod.rs` no longer owns the autonomous GitHub runtime path through
  `run_repo_sprint`.
- `ready_to_merge` is quiescent.
- Auto merge, when enabled, creates exactly one `merge_pr` activity per stable
  PR head SHA.
- The workflow reaches `done` only after GitHub confirms the PR is merged or the
  issue is otherwise closed.
- Repeated polling after `done` dispatches zero agents.
- `cargo test --workspace`, `cargo fmt --all -- --check`, and
  `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` pass.
