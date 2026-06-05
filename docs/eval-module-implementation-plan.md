# Eval Module Implementation Plan

Status: Draft
Date: 2026-06-05
Related spec: [`docs/eval-module-design.md`](eval-module-design.md)

## Goal

Deliver a first usable eval layer that can score one live PR repair run from
persisted evidence. The first release should prove the architecture before it
tries to automate large benchmark suites.

## Work Breakdown

### Issue 1: Add the pure `harness-eval` crate

Scope:

- Add `crates/harness-eval`.
- Define serializable eval run, PR snapshot, runtime snapshot, usage snapshot,
  hard gate, scorecard, reviewer judgment, and quality snapshot types.
- Implement deterministic hard gate evaluation.
- Implement deterministic score aggregation and grade caps.
- Add fixture tests for ready/no-op, unresolved review threads, stale head,
  missing runtime artifact, reviewer head mismatch, and score totals.

Out of scope:

- Database storage.
- GitHub API collection.
- LLM judge execution.
- Dashboard UI.

Acceptance:

- `cargo test -p harness-eval` passes.
- The crate has no network, shell, or database dependency.
- Root workspace builds with `cargo check`.

### Issue 2: Fix and harden the external PR repair evaluator

Scope:

- URL-encode task IDs before polling `/tasks/{task_id}`.
- Always write `baseline_pr.json`, `final_pr.json`, `task_body.json`,
  `submission.json`, `task_detail_final.json`, and `summary.md`.
- Add a machine-readable `quality_snapshot.json` using the `harness-eval` schema
  once the crate exists.
- Keep collect-only mode working without a Harness server.

Acceptance:

- `bash -n scripts/evaluate_pr_repair.sh` passes.
- A task ID containing `/` is encoded as a single URL path segment.
- Collect-only mode still writes the baseline report.

### Issue 3: Persist eval runs in `harness-server`

Scope:

- Add eval run and artifact storage.
- Add migrations for `eval_runs`, `eval_artifacts`, and `quality_snapshots`.
- Add API routes for run creation, artifact upload, scoring, snapshot lookup, and
  latest snapshot by PR.
- Score only from already-ingested artifacts.

Out of scope:

- Server-side GitHub GraphQL collection.
- Running LLM judges.
- Cost price catalog changes.

Acceptance:

- API tests cover creating a run, uploading baseline/final artifacts, scoring a
  run, and fetching the latest snapshot for a PR.
- Raw prompt text is not stored.

### Issue 4: Join usage attribution into eval snapshots

Scope:

- Read usage rollups by `workflow_id`, `runtime_job_id`, and
  `agent_invocation_id`.
- Include token and cost confidence labels.
- Penalize missing attribution without inventing prices.

Acceptance:

- Eval snapshots show exact, estimated, observed, or unknown confidence.
- Missing price catalog leaves cost fields null with diagnostics.

### Issue 5: Add the dashboard eval view

Scope:

- Add an `Evals` dashboard route.
- Show latest runs, hard gates, score breakdown, final PR state, runtime links,
  usage totals, confidence labels, and failure taxonomy.

Acceptance:

- Operators can tell whether a PR repair succeeded, why it failed, and what it
  cost without reading raw logs.

## First PR Scope

The first PR should include only:

- `docs/eval-module-design.md`
- `docs/eval-module-implementation-plan.md`
- `docs/pr-repair-capability-evaluation.md`
- `scripts/evaluate_pr_repair.sh` URL-encoding fix
- optional `crates/harness-eval` pure crate if it is ready and tested

Do not include unrelated usage dashboard, README, or AGENTS changes in this PR.

## Execution Order

1. Create the tracking issue from this plan.
2. Land the spec and evaluator URL-encoding fix.
3. Land the pure `harness-eval` crate.
4. Run one collect-only baseline against a live PR.
5. Run one live PR repair eval and produce `quality_snapshot.json`.
6. Add server persistence and APIs.
7. Add dashboard visibility.

