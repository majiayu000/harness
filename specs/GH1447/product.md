# Product Spec

## Linked Issue

GH-1447

## User Problem

The maintainer changes prompts, retry policies, or workflow logic and cannot
measure whether the change improved or regressed agent outcomes. The previous
eval attempt (harness-eval crate, eval_store) recorded zero production runs and
is scheduled for deletion in GH-1434 because it lived outside the real traffic
path. Without a regression eval loop, every harness change ships on intuition.

## Goals

- Replay a curated set of real, already-resolved issues through the normal
  workflow runtime dispatch path.
- Report pass@1, pass^k, cost per task, and tokens per task for a harness
  version, diffable against a previous version.
- Score from durable evidence (validation commands, CI status, diff scope,
  merge outcome), not from LLM judgment.

## Non-Goals

- Reviving the harness-eval crate, eval_store schema, or any standalone eval
  layer (contradicts GH-1434).
- LLM-as-judge as a primary scoring signal.
- Public benchmark (SWE-bench) submission tooling.
- Automatic merge of eval-produced PRs.

## Behavior Invariants

1. An eval run consumes a benchmark manifest listing issue references with
   pinned base commits and expected verification commands.
2. Each benchmark case dispatches through the same runtime path as production
   issue work; no eval-only execution shortcut exists.
3. Eval-produced branches and PRs are clearly marked and never auto-merged;
   cleanup removes all eval workspaces at terminal state.
4. Scoring uses only recorded evidence; two scorings of the same evidence
   produce identical results.
5. The report includes pass@1, pass^k (k configurable, default 3), total and
   per-case cost, and tokens; missing evidence fails the case, it never
   silently passes.
6. Results persist as workflow runtime records queryable by harness version
   tag; no new storage namespace is created.
7. A comparison of two eval runs lists per-case status transitions
   (pass→fail, fail→pass) and aggregate deltas.
8. A cancelled or crashed eval run leaves no orphan workflows, workspaces, or
   schemas (reuses existing reaper guarantees).

## Acceptance Criteria

- [ ] Benchmark manifest format documented; 10+ historical harness issues curated.
- [ ] One command runs the set and emits the metric report.
- [ ] Same-evidence rescoring is byte-identical (determinism test).
- [ ] Version-to-version diff report implemented and demonstrated.
- [ ] Zero new crates; zero new Postgres schemas outside the workflow namespace.

## Edge Cases

- Benchmark issue's pinned base commit no longer builds — case reports
  `infra_failed`, excluded from pass-rate denominator with explicit count.
- Agent opens no PR — case fails with `no_submission` evidence class.
- Budget cap hit mid-case — case fails with `budget_exceeded`; run continues.
- Two eval runs launched concurrently — second is rejected or queued; never
  interleaved into the same report.

## Rollout Notes

Ships behind a CLI/API entry that operators invoke manually; no scheduled eval
in this phase. Depends on GH-1434 Phase 1 landing first so scoring semantics
are ported from, not linked against, the deleted crate.
