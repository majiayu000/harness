# Workflow State Machine Quality Design

Status: Draft
Date: 2026-06-06
Audience: Harness maintainers and operators

## Purpose

Harness should improve issue and PR repair quality by making workflow state,
activity evidence, review gates, and evaluation artifacts authoritative. The
goal is not to give the agent a larger prompt. The goal is to make the runtime
force the right sequence of decisions and make false success impossible.

This design is intentionally written before implementation. It records the
architecture search, the current Harness gaps, the target state machine, and a
phased implementation plan.

## Research Summary

The relevant external patterns are durable execution, orchestrated sagas,
statecharts, persisted human-in-the-loop checkpoints, and trace/eval based
agent improvement.

### Durable Execution

Temporal's durable workflow model is the closest reference, but Harness should
copy the invariants rather than adopt Temporal as a dependency right now.

Useful invariants:

- Workflow history is the source of truth.
- Reducers must be deterministic under replay.
- Commands and activity results must be persisted.
- External side effects belong in activities, not in reducer logic.
- A replay must reuse recorded activity results instead of recomputing external
  work.

Harness already has analogous tables and concepts: workflow instances, events,
commands, runtime jobs, and activity results. The design should strengthen those
invariants locally instead of introducing a second workflow engine.

Reference: https://docs.temporal.io/workflows
Reference: https://docs.temporal.io/workflow-definition
Reference: https://docs.temporal.io/activities

### Saga Orchestration

Issue implementation and PR repair are sagas: they span GitHub state, local
worktrees, CI, review, mergeability, and operator approval. The correct model is
not "one LLM turn does everything"; it is an orchestrated sequence of idempotent
steps, hard gates, and retryable follow-up actions.

Useful invariants:

- The orchestrator owns the state.
- Each local transaction records evidence.
- Failed steps either retry idempotently, compensate, or block with a clear
  operator action.
- The pivot point should be explicit. For Harness, pushing a PR branch is a
  pivot: later steps must validate and converge the PR rather than pretending
  the work can be silently undone.

Reference: https://learn.microsoft.com/en-us/azure/architecture/patterns/saga

### Persisted Human-in-the-Loop

Human or reviewer intervention must be a durable state, not an in-memory pause
or an agent narration. LangGraph's interrupt/checkpoint design is useful here:
pause state is persisted, the thread ID is the resume cursor, and resumed
execution continues from persisted state.

Harness equivalent:

- `local_review_gate`, `awaiting_operator_approval`, and `blocked` states must
  carry structured evidence and a resume action.
- The dashboard should show exactly which gate is waiting and what evidence is
  missing.

Reference: https://docs.langchain.com/oss/python/langgraph/persistence
Reference: https://docs.langchain.com/oss/python/langgraph/human-in-the-loop

### GitHub Merge Gates

GitHub branch protection concepts map directly to Harness final gates:

- Required PR reviews.
- Required status checks.
- Conversation resolution.
- Merge queue or up-to-date branch requirements.
- Deployment gates when configured.

Harness must collect these facts from current GitHub state, not from the agent's
claim. Active unresolved review threads, stale head SHA evidence, failed checks,
and dirty mergeability must block readiness.

Reference: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches

### Agent Evals

OpenAI's agent-eval guidance points to a useful order: start with traces while
debugging behavior, then move to repeatable datasets/eval runs when comparing
changes. Harness should follow that split:

- Traces and runtime events for debugging individual failures.
- `QualitySnapshot` and PR repair evals for before/after scoring.
- Deterministic gates first; LLM or human judgment only for subjective code
  quality.

Reference: https://platform.openai.com/docs/guides/agent-evals

## Current Harness Gaps

### G1: Planning Is Reactive Instead of Normal

`github_issue_pr` allows `scheduled -> planning -> implementing`, and the code
already has `replan_issue`. However, ordinary issue submission currently moves
directly into `implement_issue`. The normal path does not require a
pre-implementation plan artifact.

Impact:

- The first mutating agent turn is doing discovery, planning, implementation,
  validation, and PR creation at once.
- `replan_issue` is used after the agent detects a plan concern, which is too
  late for many quality failures.

### G2: PR Repair Has Two Ownership Paths

Issue-owned PR feedback uses the `github_issue_pr` lifecycle. Standalone open
PR feedback discovered by `repo_backlog` can become a `prompt_task`.

Impact:

- The same "repair PR feedback" work can receive different gates depending on
  how it was discovered.
- `prompt_task` is intentionally generic, so it should not be the owner for
  merge-relevant PR repair unless it explicitly delegates into a PR lifecycle.

### G3: PR Readiness Can Still Trust Agent Signals Too Early

Recent prompt contracts require final PR state refreshes, but the reducer must
own the hard gate. `PrReadyToMerge`, a valid-looking `workflow_decision`, or an
agent summary should not be enough to move a PR lifecycle forward unless a
machine-readable PR snapshot proves the current head, active unresolved review
threads, checks, review state, and mergeability.

Impact:

- The workflow can report progress from an agent conclusion rather than current
  GitHub truth.
- A stale review or check result can look valid when the PR head changed after
  that evidence was collected.
- The operator cannot distinguish "ready because GitHub facts prove it" from
  "ready because the agent said so."

### G4: Quality Gate Exists but Is Not the Main Lifecycle Gate

`quality_gate` exists as a workflow, but the issue/PR lifecycle primarily relies
on implementation validation, local review, remote feedback inspection, and CI.

Impact:

- Operators can see a `quality_gate` workflow but not know whether it is
  mandatory for issue/PR readiness.
- Quality scoring and workflow state can drift.

### G5: Prompt Contracts Still Carry Too Much Authority

Recent hard gates improved local review and final remote refresh requirements,
but some success criteria are still expressed as "must include" prompt text.

Impact:

- Agent narration can satisfy the wording while missing machine-verifiable
  evidence.
- Reducers should reject missing evidence, not merely ask the agent for it.

### G6: Readiness Is Not a Single Auditable Object

The system has workflow state, runtime jobs, PR metadata, validation records,
usage attribution, and eval scoring, but the operator needs one final quality
object.

Impact:

- A merged PR can still hide poor quality.
- A blocked PR can still be useful, but only if the blocker is precise and the
  cost is bounded.

## Design Principles

1. Runtime state is authoritative; agent text is evidence, not authority.
2. Reducers are deterministic and side-effect free.
3. Activities own side effects and return structured artifacts/signals.
4. Every mutating step has a typed evidence contract.
5. PR readiness is a hard gate backed by current GitHub evidence.
6. Human/local review is a durable workflow state.
7. Eval scoring is separate from execution, but linked by workflow/job IDs.
8. One live PR branch has one mutating owner at a time.
9. Standalone PR feedback should converge into a PR lifecycle, not stay a free
   prompt task.
10. "10/10" means hard gates pass, code-quality score is high, and cost/runtime
    behavior is bounded.

## Target Workflow Model

### Workflow Types

Keep these workflow types:

- `repo_backlog`: intake and prioritization only.
- `github_issue_pr`: owner lifecycle for issue implementation and PR repair.
- `pr_feedback`: remote inspection child workflow only, not a repair owner.
- `prompt_task`: generic operator prompt tasks, not default PR repair.
- `quality_gate`: reusable validation/eval gate, explicitly wired where used.

Do not collapse everything into one workflow immediately. The risk is too high.
Instead, make ownership explicit:

- Discovery creates or updates owner workflows.
- Owner workflows enqueue mutating activities.
- Inspection child workflows report signals back to owners.
- Evaluation observes and scores; it does not drive normal execution.

### `github_issue_pr` Statechart

```text
discovered
  -> awaiting_dependencies
  -> planning
  -> implementing
  -> pr_open
  -> local_review_gate
  -> awaiting_feedback
  -> addressing_feedback
  -> local_review_gate
  -> awaiting_feedback
  -> quality_gate_pending
  -> ready_to_merge
  -> awaiting_operator_approval
  -> done

terminal alternates:
  -> blocked
  -> failed
  -> cancelled
```

The first implementation slice should not add every state. The important
direction is the invariant:

```text
plan before first mutation
local review before remote-ready claim
remote evidence before ready_to_merge
quality snapshot before done/merge
```

### `repo_backlog` Statechart

```text
idle
  -> scanning
  -> planning_batch
  -> dispatching
  -> idle

side exits:
  -> reconciling
  -> blocked
  -> failed
  -> cancelled
```

Rules:

- `poll_repo_backlog` should become deterministic GitHub metadata collection
  over time.
- LLM use should be limited to ambiguous prioritization or sprint planning.
- Open PR feedback should start or bind a PR lifecycle, not a generic prompt
  task, once the PR ownership path exists.

### PR Feedback Ownership

Remote PR feedback inspection should be read-only:

```text
pr_feedback:
  pending -> inspecting -> feedback_found | no_actionable_feedback | ready_to_merge
```

Mutating repair belongs to the parent:

```text
github_issue_pr:
  awaiting_feedback -> addressing_feedback -> local_review_gate
```

### PR Repair Evidence Gate

The immediate quality fix is a server-enforced evidence gate for PR repair and
readiness. The first slice validates a machine-readable GitHub snapshot returned
by the runtime activity. A later slice should replace this with a server-owned
GitHub snapshot collector so the reducer no longer trusts agent-produced PR
metadata. The gate is intentionally narrower than a full state-machine rewrite.

Affected activities:

- `inspect_pr_feedback`
- `sweep_pr_feedback`
- `address_pr_feedback`

`run_local_review` stays in the existing loop for Phase 1. It already requires a
local-review outcome signal; head/diff review evidence can be added later as a
separate local-review quality gate.

Required snapshot fields:

- `pr_number`
- `pr_url`
- `head_sha` or `head_oid`
- `observed_at`
- active unresolved review-thread IDs and count
- `statusCheckRollup` state
- `mergeStateStatus`
- `reviewDecision`
- `isDraft`
- changed files
- validation commands and results when code changed
- action taken, or explicit no-code-change reason

Reducer rule:

- Parse domain evidence before considering an agent `workflow_decision`.
- Block successful PR feedback/readiness outputs that lack a current snapshot.
- Accept `PrReadyToMerge` only when the snapshot proves final-head checks,
  mergeability, review approval, non-draft state, and active review-thread
  gates.
- Accept `address_pr_feedback` success only when the activity proves a pushed
  head, a reply/resolution action, or an explicit no-op reason plus validation.

### Quality Gate Position

`quality_gate` should be explicit:

```text
awaiting_feedback -> quality_gate_pending -> ready_to_merge
```

The gate should consume:

- final PR snapshot
- validation records
- local review result
- remote review thread status
- check status
- mergeability
- changed files
- runtime job status
- usage/cost attribution when available

## Evidence Contracts

### `plan_issue`

Purpose: force a non-mutating planning pass before first implementation.

Required output:

- `IssuePlanReady` signal or `issue_plan` artifact.
- issue number and repo.
- problem summary.
- planned files or areas.
- risk level.
- validation plan.
- explicit no-code path if the issue is already closed/resolved.

Reducer behavior:

- Missing plan evidence blocks.
- Valid plan moves `planning -> implementing` and enqueues `implement_issue`.
- `force_execute` may bypass `plan_issue` for emergency/manual override.

### `pr_repair_snapshot`

Purpose: make PR repair and readiness decisions machine-verifiable.

Required output:

- PR identity: repo, PR number, URL.
- Head identity: final head SHA/OID and observed timestamp.
- Review truth: active unresolved review threads, resolved/outdated status, and
  review decision.
- Check truth: status check rollup state and failing check names when present.
- Merge truth: merge state status.
- Draft truth: `isDraft` or equivalent.
- Diff truth: changed files and whether they are in scope.
- Action truth: pushed commit, review-thread reply/resolution, or explicit
  no-code-change reason.
- Validation truth: commands run and their pass/fail/blocked status when code
  changed.

Reducer behavior:

- Missing snapshot blocks PR readiness and PR feedback repair success.
- Stale or headless snapshot blocks readiness.
- Non-zero active unresolved review threads block `PrReadyToMerge`. A justified
  rejection can be recorded as a blocker or operator-approval request, but it
  must not move directly to `ready_to_merge`.
- Failed checks block readiness.
- Dirty mergeability blocks readiness.
- Missing or non-approved review decision blocks readiness.
- Draft PR state blocks readiness.
- A no-code-change result is allowed only for ready/no-op controls or
  explicitly false-positive feedback.

### `implement_issue`

Required output:

- PR artifact with `pr_number`, `pr_url`, and ideally `head_sha`; or
- closed/resolved issue evidence; or
- blocked/fatal result with reason.

Reducer behavior:

- Empty success blocks.
- PR artifact binds PR and moves toward local review.
- Closed issue evidence can finish the workflow.

### `run_local_review`

Required output:

- `LocalReviewPassed`
- `LocalReviewChangesRequested`
- `LocalReviewBlocked`

Reducer behavior:

- Missing local-review outcome blocks.
- Changes requested returns to `addressing_feedback`.
- Passed moves to remote feedback/readiness inspection.

### `inspect_pr_feedback`

Required output:

- `FeedbackFound`
- `ChangesRequested`
- `ChecksFailed`
- `NoFeedbackFound`
- `PrReadyToMerge`

Reducer behavior:

- Blocking signals return to `addressing_feedback`.
- No feedback waits.
- Ready signal is accepted only when the PR snapshot proves final head,
  approval, non-draft state, successful checks, clean mergeability, and zero
  active unresolved review threads.

### `quality_gate`

Required output:

- validation evidence
- final PR snapshot
- no active unresolved review threads
- checks passing for final head
- mergeability clean
- runtime artifact completeness

Reducer behavior:

- Any missing hard gate blocks.
- Passing gate can move to `ready_to_merge`.

## Quality Score Model

Use the existing eval module and PR repair evaluation rubric as the scoring
base. Do not replace it with a new scoring system.

`10/10` should mean:

- all hard gates pass
- numeric score is at least 95/100
- code-quality subscore is at least 20/22
- final head SHA is named by validation/review evidence
- no active unresolved non-outdated review thread remains
- checks are successful for final head
- no unrelated PR is opened
- no duplicate running workflow/job remains
- usage/cost attribution is observed or exact

Hard gates cap the score. If a run claims success with stale head evidence,
missing runtime artifacts, unresolved review threads, or wrong PR targeting, it
cannot be an A-grade run even if the PR later merges.

## Implementation Plan

### Phase 0: No-Code Design and Baseline

Deliverables:

- This design document.
- Baseline collect-only snapshots for open Harness PR candidates.
- Current best historical baseline, for example the existing `#1235` eval
  score if available in `docs/pr-repair-evals`.

Validation:

- No runtime code changes.
- `git diff` should show only this design doc.

### Phase 1: PR Repair Evidence Gate

Deliverables:

- Add a `pr_repair_snapshot` or `pr_feedback_snapshot` artifact contract.
- Make `inspect_pr_feedback` and `sweep_pr_feedback` block successful output
  that has no feedback outcome snapshot.
- Make `PrReadyToMerge` require current head, active review-thread count,
  checks, approved review decision, non-draft state, and mergeability evidence.
- Make `address_pr_feedback` success require pushed-head evidence,
  reply/resolution evidence, or explicit no-code-change reason plus validation.
- Make reducer domain evidence checks run before accepting agent-provided
  `workflow_decision` artifacts for PR feedback/readiness transitions.
- Keep the existing local review loop intact.

Non-goals:

- Do not change issue submission into a new `plan_issue` phase in this slice.
- Do not make `run_local_review` require head/diff evidence in this slice.
- Do not claim the reducer owns remote GitHub truth until a server-side snapshot
  collector exists.

Validation:

- `cargo fmt --all -- --check`
- focused `harness-workflow` reducer tests:
  - ready signal without snapshot blocks
  - valid workflow decision without snapshot blocks
  - address feedback empty success blocks
  - valid snapshot allows the intended transition
- focused `harness-server` prompt packet tests for the new snapshot contract

Expected quality effect:

- Fewer false ready-to-merge claims.
- Fewer PR repair runs that finish while review threads/checks remain blocked.
- Better eval artifacts for current-head scoring.

### Phase 1.5: Server-Owned PR Snapshot Collector

Deliverables:

- Add a GitHub API/GraphQL snapshot collector outside agent prompts.
- Persist snapshot source, observed timestamp, PR head, review decision, draft
  state, review-thread count, check rollup, and mergeability.
- Bind `ready_to_merge` to the latest persisted snapshot for the final head.
- Reject agent-provided PR readiness metadata when a fresher server snapshot
  disagrees.

Validation:

- Unit tests for snapshot parsing and stale/head mismatch rejection.
- Reducer tests proving a stale agent snapshot cannot override server truth.
- Live collect-only eval against an open PR.

Expected quality effect:

- Removes the remaining trust gap where an agent can produce a plausible but
  stale or incorrect snapshot.

### Phase 2: Pre-Implementation Plan Gate

Deliverables:

- Add `plan_issue` activity constants and contract.
- Change non-`force_execute` issue submission to `planning`.
- Add reducer logic for `plan_issue` success/failure.
- Pass `issue_plan` summary into `implement_issue` command payload.
- Update tests from direct `implementing` to `planning`.
- Keep `force_execute` direct-to-implementation behavior.

Validation:

- `cargo fmt --all -- --check`
- `cargo test -p harness-workflow issue_submission`
- `cargo test -p harness-workflow plan_issue`
- focused server submission tests that assert `planning -> implement_issue`

Expected quality effect:

- Fewer first-turn broad edits.
- Better validation planning.
- Cleaner failure mode when the agent cannot identify a safe path.

### Phase 3: PR Repair Ownership Cleanup

Deliverables:

- Introduce `pr_repair` lifecycle or route standalone PR feedback into
  `github_issue_pr` with a PR subject.
- Stop using generic `prompt_task` as the default repair owner for open PR
  feedback discovered by backlog.
- Preserve `prompt_task` for explicit manual prompt tasks.

Validation:

- repo backlog tests for open PR feedback dispatch.
- runtime worker child workflow tests.
- live collect-only eval on an open PR.

Expected quality effect:

- Same PR repair gates regardless of discovery path.
- Better attribution and fewer false terminal states.

### Phase 4: Quality Gate Wiring

Deliverables:

- Wire `quality_gate` into the issue/PR lifecycle before `ready_to_merge`.
- Require `QualitySnapshot` or equivalent hard-gate evidence.
- Expose quality gate status in task detail and runtime tree.

Validation:

- quality gate reducer tests.
- local review completion gate tests.
- eval scorecard fixture tests.

Expected quality effect:

- Merge readiness becomes an auditable runtime fact.
- "Merged" and "good" become separate but connected facts.

### Phase 5: Eval-Driven Optimization Loop

Deliverables:

- Select one small PR candidate at a time.
- Run collect-only baseline.
- Run one Harness repair with bounded turns.
- Collect final snapshot.
- Score with existing eval rubric.
- Commit only if score improves or the runtime blocks more correctly.

Validation:

- Compare against historical baseline.
- Require score improvement before committing prompt/runtime changes.
- Record failed attempts as eval artifacts, not silent experiments.

## Rejected Alternatives

### Adopt Temporal Directly

Rejected for now. Temporal is a strong reference, but adopting it would create a
dual orchestration system while Harness already has workflow instances, events,
commands, runtime jobs, and reducers. The near-term risk is integration churn,
not missing conceptual machinery.

### One Giant LLM Workflow

Rejected. A single large prompt could plan, implement, review, repair, and
score, but it would destroy evidence boundaries and make retries/attribution
unclear.

### Only Improve Prompts

Rejected as insufficient. Prompt wording helps, but the recurring failure mode
is false success. False success requires reducer-level hard gates and current
remote evidence.

### Make `repo_backlog` the Owner

Rejected. `repo_backlog` should discover and dispatch. If it owns repair state,
repo-level polling becomes entangled with per-PR lifecycle, retries, and merge
readiness.

## Open Questions

1. Should `plan_issue` be required for all non-`force_execute` issues, or only
   for medium/high risk labels?
2. Should standalone PR repair become a new `pr_repair` workflow, or should
   `github_issue_pr` support `WorkflowSubject::new("pr", "pr:<number>")`?
3. Should `quality_gate` be a child workflow or an inline state in
   `github_issue_pr`?
4. What budget cap should define "10/10" for small, medium, and large PR repair?
5. Should operator approval be required before merge, or is `ready_to_merge`
   sufficient for current automation scope?

## First Slice Recommendation

Implement Phase 1 first: PR repair evidence gate.

Reason:

- It directly addresses the user's current quality problem: real PR repair
  should not claim success from agent narration.
- It is small enough to test locally.
- It does not require a full state-machine rewrite.
- It preserves existing PR feedback/local review improvements.
- It creates a clear before/after eval hypothesis.

Success criteria:

- `PrReadyToMerge` without current PR snapshot blocks.
- `address_pr_feedback` empty success blocks.
- A valid PR snapshot can move to the intended next state.
- Existing local review tests continue to pass.
- A collect-only/live eval can show fewer false-success states or a higher
  PR repair score.
