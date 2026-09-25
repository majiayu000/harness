# Native PR workflow real Cursor pilot — 2026-09-10

Outcome: **automatic local review → repair/push → independent re-review verified;
full workflow failed at the extra server validation gate**.

## Scope and identity

- Evaluation PR: [majiayu000/harness-cursor-eval-20260910-152946#1](https://github.com/majiayu000/harness-cursor-eval-20260910-152946/pull/1) (private, isolated, left open).
- Frozen candidate start: `2abc521f947cfdb059dbe28a42a5347829391e55`.
- Base: `f3d81f14353034921fef7c0c1b2fc72a0f4b8ae8` on `eval-base`.
- Actual final pushed head: `862f99cba2acfd35883a60b90ae3b2f1bfdedc79`.
- Exactly one initial structured `repo` + `pr` submission through
  `/api/workflows/runtime/submissions`; no evaluator repair/review resubmission.
- Dedicated server/database and worktree. Original Loom PR was not modified.
- Cursor model `auto`; usage/cost unknown. Four actual Cursor sessions ran.

## Observed native sequence

| Activity | Duration | Execution status |
|---|---|---|
| run_local_review | 196.7 s | succeeded |
| address_pr_feedback | 190.7 s | succeeded |
| run_local_review | 113.7 s | succeeded |
| start_child_workflow | 2.0 s | succeeded |
| run_quality_gate | 151.6 s | failed |

The first local reviewer reproduced the actual mixed Vitest/coverage-v8 failure
and requested changes. The workflow automatically dispatched a different Cursor
session to repair and push. A third session independently reviewed the final
head and approved the panel changes. The native workflow then started its quality
gate. Despite the Agent's passing checks, server-side validation superseded the
result with `validation_commands_missing`; both parent and child ended failed.
No timeout, fabricated Agent response, or manual state mutation produced this
sequence. No production fix to the gate was made during this baseline run.

## Independent result checks

The evaluator fetched the actual pushed head into a separate panel checkout and
ran the repository's frozen-lockfile install, typecheck, tests, and coverage.
All exited 0; the real test suite reported 193 passing tests. This verifies the
panel repair, not the entire Rust application or all release requirements.
GitHub reported no status checks on the evaluation PR, which is missing CI
coverage, not a CI pass. The PR remains open and was not merged.

## External review and limits

A GitHub `chatgpt-codex-connector` review appeared automatically in this new
repository. The repair summary says it addressed local-review/Codex feedback and
resolved a thread. The local Cursor reviewer independently reproduced the same
version mismatch, but this is not a bot-free comparison; record the external
advisor as an uncontrolled input. All Harness coding/review activities still
used Cursor. The evaluator did not request or write the bot's findings.

This pilot starts with a real PR submission, so it does not verify automatic
GitHub discovery/intake. It covers one repair round, one development case, and
one worker; it does not establish multi-round convergence, held-out quality,
20-way concurrency, merge readiness, or an improved prompt policy. Full ancestry
of the historical commit was needed to publish the isolated branch after an
initial shallow-push failure; later upstream solution commits were not fetched.

## Evidence

Local directory: `/Users/apple/.local/share/harness/cursor-evals/20260910-152942-native`.

- `evidence/submission-input.json` and `submission.json`: the only initial task.
- `evidence/runtime_jobs.json` and `workflow_instances.json`: actual automatic
  activities, state outcomes, Cursor thread IDs, and transcript references.
- `evidence/github-final.json`: real pushed head, changed files and review sources.
- `evidence/evaluator-checks.json` and logs: fresh independent execution results.
- `evidence/final-panel.tar`: candidate panel at the actual pushed head.

This is a reproducible failure baseline for the unwanted post-review gate. Do not
count it as an end-to-end success merely because the repair was pushed or the
three earlier activities succeeded.
