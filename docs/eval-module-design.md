# Eval Module Design

Status: Draft
Date: 2026-06-05
Audience: Harness maintainers and operators

## Purpose

Harness needs a durable evaluation layer for agent work quality. A task is not
good merely because it reached a terminal state, opened a PR, or merged. The eval
module should prove what happened, whether it satisfied objective gates, how good
the repair was, and how much agent work it consumed.

This design creates a dedicated `harness-eval` module for reusable evaluation
models and scoring logic. It keeps fact collection, scoring, LLM judgment,
storage, and dashboard rendering separated so Harness does not grade itself by
agent narration.

## Existing Pieces

Harness already has three related but incomplete pieces:

- `scripts/evaluate_pr_repair.sh` collects live PR facts, submits a Harness task,
  and writes local artifacts for PR repair evaluation.
- `crates/harness-observe/src/quality.rs` grades generic hook/event quality. It
  is not a PR repair or workflow outcome evaluator.
- `crates/harness-protocol/src/contract.rs` defines `SprintContract` and
  `EvalResult` for per-implementation acceptance loops. That is an inner task
  contract, not an independent operator-facing capability evaluation.

The new eval module should reuse those concepts where useful, but it should not
collapse them into one "quality" bucket.

## Design Goals

- Evaluate Harness runs independently from agent self-reports.
- Keep deterministic hard gates authoritative.
- Use LLM or human review only for subjective code-quality and trajectory
  judgment.
- Persist enough evidence to audit a run later.
- Attribute usage and cost to the exact workflow, runtime job, and agent
  invocation.
- Support one PR repair run at a time for early live evals, while allowing
  historical comparison across many runs.
- Avoid raw prompt storage and avoid shelling out to `gh` or `git` from Harness
  Rust crates.

## Non-Goals

- Do not replace GitHub CI, code review, or branch protection.
- Do not use a single LLM judge as the source of truth for merge readiness.
- Do not hardcode model prices in Rust.
- Do not require provider billing APIs for the first release.
- Do not make the workflow runtime depend on eval scoring to execute normal work.

## Architectural Decision

Create a new workspace crate:

```text
crates/harness-eval/
```

The crate owns pure evaluation types, deterministic gates, scoring, grade caps,
and report rendering. It does not own external collection. It must not run
`gh`, `git`, or provider CLIs.

Server integration lives in `harness-server`:

```text
crates/harness-server/src/eval/
```

The server integration stores snapshots, exposes read-only APIs, and links eval
runs to workflow runtime state. It can ingest snapshots produced by external
tools such as `scripts/evaluate_pr_repair.sh`.

Dashboard integration lives in the web app:

```text
web/src/routes/dashboard/Evals.tsx
web/src/types/eval.ts
```

## Module Boundaries

```text
external collector
  - GitHub GraphQL snapshots
  - local script artifacts
  - optional LLM/human review output
  - uploads normalized input

harness-eval
  - typed input/output schemas
  - deterministic hard gates
  - scorecard calculations
  - grade caps
  - report rendering
  - no network, no shell, no database

harness-server eval integration
  - stores eval runs and artifacts
  - reads workflow/runtime state
  - joins usage attribution
  - exposes eval APIs

dashboard
  - shows outcome, score, gates, usage, and blockers
  - never hides confidence labels
```

This keeps the evaluator testable and makes collection replaceable. The first
collector can be a script using `gh api graphql`; later collectors can use an
internal GitHub API client or webhook-derived snapshots without changing the
score model.

## Core Data Model

### `EvalRun`

One invocation of an evaluation scenario.

```rust
pub struct EvalRun {
    pub run_id: EvalRunId,
    pub scenario: EvalScenario,
    pub target: EvalTarget,
    pub status: EvalRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub harness_task_id: Option<String>,
    pub workflow_id: Option<String>,
    pub runtime_job_ids: Vec<Uuid>,
    pub agent_invocation_ids: Vec<Uuid>,
    pub artifacts: Vec<EvalArtifactRef>,
}
```

### `EvalScenario`

```rust
pub enum EvalScenario {
    PrRepair,
    IssueImplementation,
    ReadyNoopControl,
    WorkflowRecovery,
}
```

`PrRepair` is the first scenario to implement. The other variants reserve the
schema for future benchmark suites.

### `EvalTarget`

```rust
pub enum EvalTarget {
    PullRequest {
        repo: String,
        pr_number: u64,
        base_ref: Option<String>,
        head_ref: Option<String>,
    },
    Issue {
        repo: String,
        issue_number: u64,
    },
    PromptTask {
        task_id: String,
    },
}
```

### `PullRequestSnapshot`

Normalized PR truth collected before and after the run.

```rust
pub struct PullRequestSnapshot {
    pub repo: String,
    pub pr_number: u64,
    pub url: Option<String>,
    pub title: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    pub is_draft: bool,
    pub merge_state: MergeState,
    pub check_state: CheckState,
    pub review_decision: Option<ReviewDecision>,
    pub active_unresolved_review_threads: Vec<ReviewThreadSnapshot>,
    pub changed_files: Vec<ChangedFileSnapshot>,
    pub collected_at: DateTime<Utc>,
}
```

`reviewThreads` must come from GraphQL or an equivalent API surface that can
distinguish active unresolved threads from outdated or resolved threads.

### `RuntimeSnapshot`

```rust
pub struct RuntimeSnapshot {
    pub task_id: Option<String>,
    pub workflow_id: Option<String>,
    pub workflow_state: Option<String>,
    pub command_ids: Vec<Uuid>,
    pub runtime_jobs: Vec<RuntimeJobSnapshot>,
    pub latest_activity: Option<String>,
    pub terminal_state: Option<String>,
    pub collected_at: DateTime<Utc>,
}
```

### `UsageSnapshot`

Usage is joined from the cost-monitoring attribution model.

```rust
pub struct UsageSnapshot {
    pub agent_invocation_id: Option<Uuid>,
    pub runtime_job_id: Option<Uuid>,
    pub workflow_id: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd_micros: Option<u64>,
    pub token_confidence: Confidence,
    pub cost_confidence: Confidence,
}
```

The eval module only consumes usage summaries. Price catalog loading remains in
the usage/cost module. Monetary values use integer micro-USD units so the pure
eval crate does not need a decimal dependency and never rounds with floats.

### `QualitySnapshot`

The final persisted report for a run.

```rust
pub struct QualitySnapshot {
    pub run: EvalRun,
    pub baseline_pr: Option<PullRequestSnapshot>,
    pub final_pr: Option<PullRequestSnapshot>,
    pub runtime: RuntimeSnapshot,
    pub usage: Vec<UsageSnapshot>,
    pub hard_gates: Vec<HardGateResult>,
    pub objective_score: ScoreBreakdown,
    pub reviewer_judgment: Option<ReviewerJudgment>,
    pub final_score: u8,
    pub final_grade: EvalGrade,
    pub grade_cap: Option<EvalGrade>,
    pub blocker_summary: Vec<String>,
}
```

## Hard Gates

Hard gates run before numeric scoring. If any required gate fails, the final
grade is capped even when other score dimensions look strong.

| Gate | Deterministic input | Failure cap |
|---|---|---|
| Target correctness | requested target vs final target | `F` |
| Branch safety | baseline/final head refs, changed PR number | `F` |
| No unrelated PR creation | collector PR list or runtime artifacts | `F` |
| Head freshness | final evidence head vs final PR head | `C` |
| Required checks | final check state for final head | `C` |
| Mergeability clean | final merge state for final head | `C` |
| Review-thread closure | final active unresolved threads | `C` |
| Runtime artifact completeness | task/workflow/job snapshots | `B` |
| Reviewer judgment freshness | reviewer head vs final PR head | `C`, or `B` when absent |
| No destructive/unrelated scope | changed files and risk scanner | `F` |

Ready/no-op control cases invert the "head changed" expectation. A correct no-op
must keep the PR head unchanged while proving current-head checks,
mergeability, and review threads.

## Scorecard

After hard gates, the score is calculated from eight dimensions:

| Dimension | Points | Source |
|---|---:|---|
| Task classification and baseline evidence | 12 | deterministic |
| Feedback discovery and prioritization | 14 | deterministic plus reviewer evidence |
| Branch and PR safety | 10 | deterministic |
| Fix correctness and scope | 22 | deterministic plus reviewer judgment |
| Verification and current-head gates | 16 | deterministic |
| Runtime workflow behavior and persistence | 12 | deterministic |
| Cost and time efficiency | 8 | deterministic |
| Reporting and attribution quality | 6 | deterministic |

The scorecard should expose each dimension separately. A single merged/not
merged status is not enough.

## Code Quality Subscore

The `Fix correctness and scope` dimension is split into objective and subjective
signals:

| Subdimension | Points | Primary evaluator |
|---|---:|---|
| Functional correctness | 6 | reviewer with tests/diff evidence |
| Edge cases and failure semantics | 4 | reviewer plus static risk checks |
| Minimal, coherent scope | 3 | deterministic diff scanner |
| Fit with existing design | 3 | reviewer |
| Test coverage for the risk | 4 | deterministic plus reviewer |
| Regression and UX/API preservation | 2 | reviewer plus API/diff scanner |

The deterministic scanner can find facts such as changed files, missing tests,
new dependencies, risky APIs, public API changes, and validation commands. It
cannot fully judge root cause, maintainability, or design fit. Those subjective
points require a structured reviewer judgment.

## Reviewer Judgment

LLM or human review output must be structured and evidence-bound:

```rust
pub struct ReviewerJudgment {
    pub reviewer_kind: ReviewerKind,
    pub judged_head_oid: String,
    pub code_quality_score: u8,
    pub trajectory_score: u8,
    pub findings: Vec<ReviewerFinding>,
    pub residual_risks: Vec<String>,
}
```

Rules:

- The judgment must name the head SHA it reviewed.
- It must cite diff paths, review threads, tests, or runtime events.
- It can subtract subjective points.
- It cannot override failed hard gates.
- It cannot approve stale-head evidence.
- A missing reviewer judgment caps the grade at `B`; a stale reviewer judgment
  fails the freshness gate and caps the grade at `C`.

## Storage Design

Start with append-only records. The schema can be normalized later when query
patterns are clear.

### `eval_runs`

| Field | Type | Notes |
|---|---|---|
| `run_id` | UUID | Primary key |
| `scenario` | Text | `pr_repair`, `ready_noop_control`, etc. |
| `target_kind` | Text | `pull_request`, `issue`, `prompt_task` |
| `target_repo` | Text | Nullable for prompt tasks |
| `target_number` | BigInt | PR or issue number |
| `status` | Text | `collecting`, `running`, `scored`, `failed` |
| `harness_task_id` | Text | Nullable |
| `workflow_id` | Text | Nullable |
| `started_at` | Timestamp | Required |
| `finished_at` | Timestamp | Nullable |

### `eval_artifacts`

| Field | Type | Notes |
|---|---|---|
| `artifact_id` | UUID | Primary key |
| `run_id` | UUID | Foreign key |
| `artifact_kind` | Text | `baseline_pr`, `final_pr`, `runtime`, `usage`, `report` |
| `sha256` | Text | Integrity check |
| `payload` | JSONB | Redacted normalized payload |
| `created_at` | Timestamp | Required |

### `quality_snapshots`

| Field | Type | Notes |
|---|---|---|
| `snapshot_id` | UUID | Primary key |
| `run_id` | UUID | Foreign key |
| `final_score` | SmallInt | `0..100` |
| `final_grade` | Text | `A`, `B`, `C`, `D`, `F` |
| `grade_cap` | Text | Nullable |
| `hard_gates` | JSONB | Full gate results |
| `score_breakdown` | JSONB | Full dimension scores |
| `reviewer_judgment` | JSONB | Nullable |
| `blocker_summary` | JSONB | Redacted strings |
| `created_at` | Timestamp | Required |

## API Design

First release APIs should be read/write for artifacts and read-only for the
dashboard.

```text
POST /api/evals/runs
GET  /api/evals/runs
GET  /api/evals/runs/{run_id}
POST /api/evals/runs/{run_id}/artifacts
POST /api/evals/runs/{run_id}/score
GET  /api/evals/quality-snapshots/{snapshot_id}
GET  /api/evals/pr/{owner}/{repo}/{pr_number}
```

`POST /api/evals/runs/{run_id}/score` runs deterministic scoring inside
`harness-eval` from already-ingested artifacts. It should not fetch GitHub or
start an agent.

## CLI and Script Flow

The current script becomes the first external collector:

```text
scripts/evaluate_pr_repair.sh
  1. collect baseline PR snapshot
  2. create eval run
  3. submit one Harness PR repair task
  4. poll task/runtime state
  5. collect final PR snapshot
  6. upload artifacts
  7. request deterministic score
  8. write local JSON and Markdown reports
```

Later, add a CLI wrapper:

```text
harness eval pr-repair --repo OWNER/REPO --pr N --server-url URL
harness eval report --run-id UUID
harness eval cases --suite pr-repair-smoke
```

## Dashboard Design

Add an `Evals` view with:

- latest eval runs by repo and scenario
- final grade and score
- hard gate status
- final head SHA, checks, and review-thread count
- runtime task/workflow/job links
- agent invocation count
- token and cost totals with confidence labels
- failure taxonomy and residual risks

The dashboard should distinguish:

- objective failure: hard gate failed
- quality failure: hard gates passed, reviewer found weak code
- workflow failure: Harness did not reach a useful terminal or waiting state
- cost failure: work succeeded but burned outside the expected envelope

## Relationship to Cost Monitoring

The eval module must not duplicate pricing logic. It should consume usage
rollups keyed by:

```text
(workflow_id, runtime_job_id, agent_invocation_id)
```

Cost confidence comes from the usage module:

- `exact` for provider or CLI token counts
- `estimated` for price catalog calculations
- `observed` for manual quota snapshots
- `unknown` when the attribution is missing

An eval run can pass functional gates and still lose cost-efficiency points when
usage attribution is missing or when repeated turns exceed the scenario budget.

## Failure Taxonomy

Every failed or weak run should classify the first meaningful failure:

| Failure kind | Meaning |
|---|---|
| `wrong_target` | Worked on the wrong issue, PR, repo, or branch |
| `stale_head` | Judged old evidence instead of final head |
| `unresolved_feedback` | Active review threads remained |
| `ci_failed` | Required checks failed on final head |
| `no_runtime_artifact` | Task/workflow/job proof missing |
| `workflow_stuck` | Runtime kept queued or active work after useful completion |
| `poor_fix_quality` | Hard gates passed but code quality was weak |
| `excessive_cost` | Too many turns, retries, or unattributed usage |
| `collector_failure` | Eval could not collect required facts |

## Test Plan

`harness-eval` should have fixture-based unit tests:

- score ready/no-op control without requiring a head change
- fail wrong-target final snapshot
- cap grade at `C` for unresolved active review threads
- cap grade at `C` for stale-head validation
- cap grade at `B` for missing runtime artifact
- cap grade at `B` when reviewer judgment is missing
- cap grade at `C` when reviewer judgment is stale
- cap grade at `F` for unrelated PR creation or destructive scope changes
- calculate score breakdown deterministically
- include both code-quality and trajectory scores in the subjective subscore
- preserve cost confidence labels

`harness-server` should have API tests:

- create eval run
- upload baseline/final artifacts
- score a run from artifacts
- fetch latest snapshot by PR
- avoid storing raw prompt text

The script should have shell or integration tests for:

- URL-encoding task IDs that contain `/`
- collect-only baseline mode
- final snapshot generation after task polling resumes

## Rollout Plan

1. Fix `scripts/evaluate_pr_repair.sh` task ID URL encoding and final snapshot
   persistence.
2. Add `crates/harness-eval` with pure schemas, hard gates, score calculation,
   and fixture tests.
3. Add server-side eval storage and APIs.
4. Wire script uploads to the server and keep local Markdown reports.
5. Join usage summaries from the cost-monitoring attribution model.
6. Add dashboard read-only eval view.
7. Add optional structured LLM/human reviewer judgment ingestion.
8. Promote a three-case PR repair suite: ready/no-op, CI repair, and
   review-thread repair.

## Acceptance Criteria

The first complete implementation is done when:

- one PR repair eval produces a persisted `QualitySnapshot`
- the snapshot includes baseline and final PR truth
- hard gates are computed by code
- the scorecard is reproducible from stored artifacts
- usage is attributed or explicitly marked unknown
- dashboard can show the latest eval for a PR
- `cargo test -p harness-eval` and relevant server API tests pass
