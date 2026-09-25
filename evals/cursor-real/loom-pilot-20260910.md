# Loom real Cursor pilot — 2026-09-10

Outcome: the frozen dependency-update problem was reproduced and repaired by real Cursor execution through Harness. A separate Cursor review was scheduled in advance with a runtime dependency and started automatically after implementation succeeded. Independent evaluator reruns confirmed the candidate result.

- Case: `loom-pr`; start `2abc521f947cfdb059dbe28a42a5347829391e55`.
- Execution: two prompt submissions on an isolated Harness server and PostgreSQL database, source workspace, Cursor model `auto`.
- Implementation: 04:23:00–04:27:18 UTC (about 4m18s).
- Review: 04:27:19–04:28:41 UTC (about 1m22s).
- Runtime thread IDs: `452aeb52-970e-4da8-bd32-320f1416aaee` and `610597f0-31df-47ac-8a09-df04649efbeb`.
- Cursor CLI: `2026.09.08-6caf4ff`.
- Actual spend/token usage: unknown.

## Independent observations

The original package versions are vitest 4.x and coverage-v8 5.x. A separate checkout of the original panel installed from the frozen lockfile successfully, then its coverage command exited 1 with `coverageFilesDirectory is required`. The repaired candidate passed typecheck, all 193 tests, and coverage; the evaluator independently reran those three commands with exit 0. No candidate test assertions were changed. The independent Cursor reviewer reported APPROVED.

The candidate aligned Vitest with coverage-v8 and adjusted test setup, TypeScript configuration, and matcher declarations. Source files remain uncommitted; the final candidate is identified by the start SHA plus saved file hashes and copies, not by claiming a new commit.

## Limits

This is one development-case pilot, not a 15-case baseline or an A/B comparison. Two prompt tasks exercise actual execution and automatic dependency handoff; they do not exercise native PR intake, the native local-review reducer, automated repair after rejection, the quality gate, GitHub submission, merge, or 20-worker concurrency. Those gaps remain. No upstream PR was changed.

The evaluator selected the case and supplied a handoff request; the result does not establish that Intent/Spec/Plan outperforms a shorter prompt. Baseline failure reproduction was independently performed after the candidate run, so this is retrospective calibration, not a pre-registered benchmark. Model auto-selection, public historical solution exposure, and dependency environment drift remain limitations.

## Evidence

Local run directory: `/Users/apple/.local/share/harness/cursor-evals/20260910-122218`.

- `evidence/implementation-input.json` and `review-input.json`: actual submitted prompts.
- `evidence/runtime-jobs.json`: real runtime outcomes and transcript references/checksums.
- `evidence/evaluator-checks.json` and corresponding logs: independent actual executions.
- `evidence/candidate-files/` and `candidate-file-hashes.json`: complete candidate identity including the new declaration file.
- `loom/EVAL_HANDOFF.md` and `loom/EVAL_REVIEW.md`: Agent-authored handoff and independent review.

The commands above are evaluator observations, not a production verification-command configuration. No simulated Agent output was used.
