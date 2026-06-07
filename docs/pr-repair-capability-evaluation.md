# PR Repair Capability Evaluation

Status: Draft
Date: 2026-06-05
Audience: Harness operators and maintainers

## Purpose

Harness PR repair quality should be measured with controlled PR tasks, not by
counting merged PRs. A good run proves that Harness understood the current PR
state, changed the right head branch, addressed the actual blocker, reran the
right validation, and stopped without creating new risk or wasting turns.

This document defines a repeatable PR repair evaluation. The companion script is
`scripts/evaluate_pr_repair.sh`.

## Capabilities Under Test

| Capability | Good signal | Bad signal |
|---|---|---|
| Review feedback repair | Blocking `reviewThreads` drop to zero on the same PR after a head-SHA-changing fix. | Harness marks done while unresolved threads remain, replies without fixing, or opens a new PR. |
| CI repair | Failing `statusCheckRollup` becomes `SUCCESS` after a focused commit on the PR branch. | Harness changes unrelated files, fails to push, or claims success while checks still fail. |
| Ready/no-op handling | Already-ready PR is left unchanged and reported as ready with current-head evidence. | Harness pushes unnecessary changes or consumes multiple turns without new evidence. |
| Stale-head protection | Any review, validation, or approval is tied to the final `headRefOid`. | Harness reuses approval/check evidence from an older head SHA. |
| Cost control | Turns, runtime age, and budget stay within the configured envelope. | Repeated same-error retries, low-priority work running while repair is blocked, or no usage attribution. |

## Live Candidate Selection

Candidate PRs must be selected from fresh GitHub GraphQL data immediately before
each run. Do not reuse candidate tables from previous sessions: head SHAs,
checks, mergeability, and active review threads can change within minutes.

Use collect-only mode to record the live baseline before every repair eval. A
good candidate set has exactly one ready/no-op control, one CI repair candidate,
and one review-feedback repair candidate.

## Evaluation Levels

### Level 0: Offline Gate Tests

Run focused tests that do not start the Harness server:

```bash
cargo test -p harness-server hosted_bot_disabled
```

This checks existing local-review completion gates:

- CI failure blocks completion.
- Stale PR head blocks completion.
- Missing local approval blocks completion.
- Closed PR blocks completion.
- Validation failure becomes actionable feedback when runtime storage exists.
- Ready-to-merge feedback is recorded only after validation, PR state, PR head,
  and PR checks gates pass.

Level 0 does not prove an agent can fix a real PR. It proves that the local gate
does not accept common false positives.

`QualitySnapshot` records a `run_mode`. Collect-only snapshots must use
`collect_only`; live Harness repair runs use `live_run` only after Harness
returns a non-empty `task_id` from `POST /tasks`. Server health failures, project
registry failures, eval API preflight failures, duplicate-task preflight
failures, POST failures, and missing task IDs are still diagnostic reports, but
they are tagged `collect_only` because no Harness repair attempt exists. The
eval API preflight must happen before `POST /tasks`; a server that is too old to
serve `/api/evals/runs` can otherwise spend agent tokens without producing a
dashboard quality record. Benchmark inputs must include an explicit
`run_mode=live_run`; missing `run_mode` is rejected because older collect-only
artifacts did not have this field.

The `score_pr_repair` CLI must be invoked with an explicit
`--run-mode live_run|collect_only` whenever it writes a new `QualitySnapshot`.
Server scoring input for `/api/evals/runs/{run_id}/score` must also include
`run_mode`. Neither path may default direct/manual scoring to `live_run`.

## Hard Gates

A run must pass every hard gate before the numeric score can be interpreted as a
success. If any hard gate fails, the maximum grade is `C` even when the agent did
some useful work.

| Gate | Pass condition | Failure meaning |
|---|---|---|
| Current PR targeting | The run updates or reports on the requested PR only. | Wrong PR, new PR, or unrelated branch. |
| Head freshness | Final validation, review, and merge-readiness evidence names the final `headRefOid`. | Stale approval or stale CI was reused. |
| Blocking feedback | Final `reviewThreads` enumeration is complete and active, non-outdated unresolved thread count is zero, or every remaining thread has an explicit justified rejection. | The original blocker remains, or the evaluator did not inspect every review thread. |
| Required checks | Final required checks are `SUCCESS` for the final head SHA, unless the case is explicitly collect-only. | CI still blocks merge. |
| Branch safety | The existing PR branch is used and no unrelated files are changed. | Cross-branch mutation, broad cleanup, or dirty-state dependence. |
| No unrelated PR creation | The run updates the existing PR branch and does not open or report a different PR. | The evaluation target was replaced by a new PR. |
| Scope containment | The final diff avoids destructive or unrelated changes outside the repair. | The run created hidden merge risk even if checks are green. |
| Reviewer judgment freshness | Optional LLM or human judgment names the final `headRefOid`; missing judgment caps the grade at `B`. | Subjective code-quality evidence is stale or absent. |
| Runtime artifact | The report includes `task_id`, `workflow_id`, runtime terminal state, and final PR snapshot. | The result cannot be audited later. |

Ready/no-op controls are exempt from the "head changed" expectation. For those
cases, a correct run can score highly without any new commit.

## 100-Point Scorecard

Use the hard gates first, then score the successful run with this rubric:

| Dimension | Points | Scoring guidance |
|---|---:|---|
| Task classification and baseline evidence | 12 | Correctly classifies review repair, CI repair, or ready/no-op and records baseline head, checks, merge state, and active threads. |
| Feedback discovery and prioritization | 14 | Finds actionable feedback, distinguishes stale or false-positive comments, and focuses on the blocker. |
| Branch and PR safety | 10 | Uses the existing PR branch, no new PR, no wrong worktree, and no unrelated local changes. |
| Fix correctness and scope | 22 | Produces the smallest correct fix, preserves product behavior, avoids silent degradation, and does not introduce regression risk. |
| Verification and current-head gates | 16 | Runs relevant local validation and confirms final-head CI plus complete active `reviewThreads` and changed-file evidence. |
| Runtime workflow behavior and persistence | 12 | Records task/workflow/job state, reaches a correct terminal or waiting state, and does not leave duplicate LLM work queued. |
| Cost and time efficiency | 8 | Completes within the expected turn/time envelope for the task class, exposes usage attribution, and avoids repeated failed runtime jobs. Repeated non-retryable runtime failures such as `configuration` or `fatal` errors receive zero points for this dimension. |
| Reporting and attribution quality | 6 | Final report proves what Harness did, what commits were pushed, what commands passed, and what remains. |

### Code Quality Subscore

The `Fix correctness and scope` dimension is the main code-quality score. Break
its 22 points down consistently so a merged PR does not hide weak engineering:

| Subdimension | Points | Full-credit signal |
|---|---:|---|
| Functional correctness | 6 | The final code actually fixes the original blocker, not just the visible reviewer wording. |
| Edge cases and failure semantics | 4 | Boundary cases, partial data, and error paths are explicit; no silent wrong totals or silent degradation. |
| Minimal, coherent scope | 3 | The diff is focused on the repair and avoids unrelated cleanup or product changes. |
| Fit with existing design | 3 | The implementation follows local patterns, types, async/runtime conventions, and dependency policy. |
| Test coverage for the risk | 4 | Tests cover the original bug class and at least one meaningful edge case introduced by the fix. |
| Regression and UX/API preservation | 2 | Existing response shape and user-visible behavior are preserved, or any intentional degradation is explicit and justified. |

Code quality is scored from the final diff, but intermediate mistakes still
matter. If the first repair introduces a real bug and only a second review cycle
finds it, subtract from functional correctness or edge-case handling even when
the final PR merges.

Grades:

| Score | Grade | Meaning |
|---:|---|---|
| 90-100 | `A` | Strong repair, low residual risk, complete evidence. |
| 80-89 | `B` | Good repair with minor evidence, runtime, or efficiency gaps. |
| 65-79 | `C` | Partial success or significant residual risk, but useful progress. |
| 50-64 | `D` | Weak progress, repeated blind retries, or poor evidence. |
| 0-49 | `F` | Wrong target, false success, destructive change, or no useful repair. |

Do not treat merge alone as a score. Merge is only one outcome after the gates
prove that the final head is ready.

### Level 1: Collect-Only Live Baseline

Collect the current GitHub truth without submitting Harness work:

```bash
scripts/evaluate_pr_repair.sh \
  --repo majiayu000/harness \
  --pr 1234 \
  --collect-only
```

This records the PR `headRefOid`, `mergeStateStatus`, `statusCheckRollup`,
`reviewDecision`, active unresolved `reviewThreads`, and a recommended task
class. Use this before every live repair run.

Collect-only reports still write `pr_repair_eval_input.json` and
`quality_snapshot.json`, but those snapshots are tagged `run_mode=collect_only`.
The benchmark CLI rejects them because no Harness repair, runtime artifact, or
usage attribution exists yet.

Live-mode preflight failures use the same `collect_only` tag until the script
successfully submits a Harness task and records its `task_id`. This keeps
environment/setup failures out of the live repair capability benchmark while
still preserving enough evidence to debug why no repair attempt started.

### Level 2: Harness Live PR Repair

Start Harness from a standalone terminal, not from a Codex session:

```bash
./target/release/harness serve \
  --transport http \
  --port 9800 \
  --project-root /Users/apple/Desktop/code/AI/tool/harness
```

Then submit one PR repair evaluation:

```bash
scripts/evaluate_pr_repair.sh \
  --repo majiayu000/harness \
  --pr 1234 \
  --server-url http://127.0.0.1:9800 \
  --project-root /Users/apple/Desktop/code/AI/tool/harness \
  --max-turns 6 \
  --max-rounds 2 \
  --timeout-secs 7200
```

Run only one live repair candidate at a time. Parallel repair tasks can mutate
overlapping runtime files and make the result hard to interpret.

### Level 3: Three-Case Capability Score

Run exactly one PR from each class:

| Case | Candidate | Expected behavior |
|---|---|---|
| Ready/no-op | `#1232` | No commit unless new evidence appears; report ready with current-head gate. |
| CI repair | `#1233` | Push a focused fix, checks move from `FAILURE` to `SUCCESS`. |
| Review repair | `#1234` or `#1235` | Push a focused fix or reply with justified evidence, active unresolved threads become zero. |

Score the run after the final GraphQL snapshot:

| Grade | Meaning |
|---|---|
| `A` | Correct fix, current-head evidence, checks green, mergeability clean, no unresolved blocking threads, bounded cost. |
| `B` | Correct fix with minor missing reporting evidence; no merge-risk remains. |
| `C` | Partial improvement, but still blocked by checks, review threads, or missing validation. |
| `D` | No meaningful progress, repeated same failure, or excessive cost. |
| `F` | Wrong branch, new PR instead of updating existing PR, false ready-to-merge, or destructive/unrelated changes. |

## Report Fields

Each run should produce a report with:

- repo and PR number
- baseline and final `headRefOid`
- baseline and final `mergeStateStatus`
- baseline and final `statusCheckRollup.state`
- baseline and final active unresolved review-thread count
- Harness `task_id`, `workflow_id`, and terminal state when available
- Runtime job `activity`, terminal state, artifact count, and `error_kind` when available
- elapsed time and configured turn/budget limits
- changed files from the final PR diff
- validation commands observed in task rounds or PR comments
- final grade and blocker summary

Each run must also write machine-readable eval artifacts:

- `pr_repair_eval_input.json`: canonical `PrRepairEvalInput` consumed by
  `harness-eval` and `/api/evals/runs/{run_id}/score`.
- `quality_snapshot.json`: deterministic `QualitySnapshot` with hard gates,
  score breakdown, grade caps, blocker summary, runtime evidence, and PR facts.
- `eval_run.json`: the server eval run created for a successfully submitted
  live Harness repair task.
- `eval_artifact_pr_repair_input.json`: the server artifact upload response for
  the final canonical input.
- `eval_score.json`: the server-side scoring response that links the eval run
  to the stored `quality_snapshot`.

The canonical input and local quality snapshot include `run_mode`. Operators
may archive collect-only artifacts next to live-run artifacts, but only
`live_run` snapshots are valid inputs for the capability benchmark. A snapshot
is `live_run` only after a Harness task was successfully submitted for the
target PR; live-mode preflight failures remain `collect_only`. The benchmark CLI
rejects missing `run_mode` rather than defaulting old snapshots to live runs.
Collect-only runs and live preflight failures are local-only; the script
persists to `/api/evals` only after the live Harness task has a `task_id`.

The Markdown summary is an operator convenience layer. It must not be the source
of truth for eval scoring, dashboards, or merge readiness.

## Interpretation Rules

- A merged PR is not automatically high quality.
- A green PR is not ready if active unresolved review threads remain.
- A green PR is not ready if `mergeStateStatus` is not `CLEAN`.
- A pushed commit is not progress if it does not reduce the original blocker.
- No-op is correct only for a ready/no-op control with current-head evidence and an unchanged PR head.
- A Harness failure can still be useful if it records a precise blocker and does
  not spend repeated turns on the same hypothesis.
- The evaluation should prefer small, representative PRs over large ambiguous
  repairs until the gate and cost dashboard are reliable.

## Next Implementation Targets

The current script can evaluate live PR repair from outside the server. The
Harness-owned implementation now has a server-side persistence path for this
same report shape as a `QualitySnapshot`. The module-level design is in
[`docs/eval-module-design.md`](eval-module-design.md):

1. Use `/api/evals/runs`, `/api/evals/runs/{run_id}/artifacts`,
   `/api/evals/runs/{run_id}/score`, and `/api/evals/pr/{owner}/{repo}/{pr}` as
   the durable eval contract.
2. Store PR repair eval reports as `quality_snapshots` linked to eval runs.
3. Attach `agent_invocation_id`, `runtime_job_id`, model, effort, and usage to
   each run.
4. Block ready-to-merge when the report has stale head SHA, failing checks, or
   unresolved active review threads.
