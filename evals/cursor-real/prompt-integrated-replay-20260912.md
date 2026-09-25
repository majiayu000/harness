# Integrated prompt workflow: real Cursor replay

Date: 2026-09-12. Verdict: real automatic review/repair/re-review verified; complete autonomous upgrade acceptance required operator clarification. No production rollout or merge.

## Runs and findings

The integrated binary was rebuilt from the previously validated combined checkout. All coding and review activities used real Cursor through native runtime submissions and isolated PostgreSQL databases. Model selection was `auto`; underlying model, tokens and cost remain unknown. No Agent outputs, review failures or CI results were manufactured.

1. Existing evaluation PR #1 at `862f99cba2acfd35883a60b90ae3b2f1bfdedc79` passed real review under the integrated contract and entered `ready_to_merge`.
2. Historical replay PR #3 in the old evaluation repository followed local review, repair and re-review, but is contaminated. Another still-running evaluation server independently discovered the PR and pushed `75a9fd0` before this run's repair could push. The repair also inspected the prior evaluation solution `862f99c`. Preserve this as operational evidence, not an independent solution success. The competing server's job was `a22e43d4-00f4-40a7-afb8-87e32935acff` in `cursor_eval_20260910170012`.
3. A new private repository, `majiayu000/harness-prompt-replay-20260912`, received only the historical starting ancestry and evaluation branches. PR #1 started at `2abc521f947cfdb059dbe28a42a5347829391e55`, base `f3d81f14353034921fef7c0c1b2fc72a0f4b8ae8`. The evaluator independently reproduced the starting coverage failure: frozen install exited 0; coverage exited 1. The known production and earlier evaluation databases contained no competing jobs for this new repository when inspected.

The initial brief emphasized compatible dependencies and working checks but did not explicitly require retaining the 5.x upgrade. Cursor's reviewer found the real peer mismatch. The automatically scheduled repair reverted coverage to 4.x, and a fresh reviewer approved that interpretation. Three distinct Cursor threads made 41 tool calls. The final `51017a5` passed independent install, typecheck, 193 tests and coverage, but did not accomplish the original upgrade objective. Do not score that result as a completed 5.x upgrade.

The evaluator then clarified the original upgrade requirement in the evaluation project's WORKFLOW.md: retain coverage 5.x and align Vitest without reverting the upgrade. A normal resubmission returned HTTP 400 because `ready_to_merge` is not eligible for feedback sweep. The evaluator used native cancellation and explicit resubmission; this is recorded operator intervention, not an autonomous retry.

## Clarified acceptance run

| Activity | Job | Outcome |
| --- | --- | --- |
| Independent review | `66449309-773c-4411-9858-00f0f7ce0576` | Rejected the prior downgrade against the explicit upgrade requirement |
| Repair | `c6a4e3f8-bf2a-488b-8358-3354fa4530f1` | Restored coverage 5.x, aligned Vitest and typings, pushed `543eb46` |
| Independent re-review | `e00ba250-4bf0-4b08-b6ba-91c868e9db82` | Approved the current SHA in a clean worktree; workflow entered `ready_to_merge` |

These three activities used distinct Cursor threads and 50 actual tool calls. Only the initial resubmission was operator-triggered; the subsequent repair and re-review were automatically scheduled. Prepared packets contain actual `local_review_result` data for repair and `previous_repair` for re-review. The clarified acceptance text was present throughout all three prepared packets.

An independent checkout fetched final head `543eb465be4b08651fcbd24d1e02e823661721d4` and ran frozen install, typecheck, tests and coverage; all exited 0. Both `vitest` and `@vitest/coverage-v8` are `^5.0.0`; 193 tests pass. No business code was edited by the evaluator. The PR remains open and the task-owned evaluation server was stopped after recording evidence.

## Remaining product and evaluation gaps

- Native PR submission currently drops the request's additional prompt: `submit_pr_feedback_to_workflow_runtime` does not pass `prepared.req.prompt` into its review/sweep requests. Issue submissions do pass `additional_prompt`. This is a confirmed code-path omission, not evidence that Cursor deliberately ignored a prompt it received. The evaluation WORKFLOW.md workaround is not a product fix.
- New operator requirements cannot directly reopen `ready_to_merge` via ordinary PR resubmission. The cancellation/resume workaround retains history but adds manual intervention.
- Reusing an evaluation repository watched by an old server permits competing owners across separate databases. Future replay setup must verify repository isolation before publishing a candidate.
- The first replay's old solution was locally reachable. Clean starts must exclude later solution refs and forbid access to other evaluation directories; prompt instructions alone do not establish a security boundary.
- Passing checks can accompany a lost product goal. Evaluator acceptance must preserve the original requested outcome, not merely require green tests. The initial brief was underspecified; the clarified run is not an uncontaminated first attempt.
- Automatic external Codex review appeared on the evaluation PRs. It is an uncontrolled advisory input. Local Cursor also reproduced the defect, but these are not bot-free experiments.

This validates one development task and a real requirement-correction loop. It does not complete the 15-case development suite, establish held-out improvement, test issue planning end-to-end, prove restart continuity or authorize broad production deployment. The product gaps above remain unfixed by this evaluation-only continuation.

## Evidence

Local root: `/Users/apple/.local/share/harness/cursor-evals/20260912-prompt-contract/`.

- `integrated-pilot/`: initial integrated review, contaminated replay, full transcripts and competing-server evidence.
- `clean-replay/`: frozen start, initial submission, original acceptance, independent baseline and reverted-result checks.
- `clean-replay/clarified/`: complete six-job history, runtime packets, workflow events and commands, final GitHub state, independent final checks.
- `clean-replay/clarified-ineligible-submission.json`, `clarified-cancel.json`, `clarified-submission.json`: exact operator intervention record.
- `clean-replay/binary.sha256`: tested binary identity.

These are local evidence artifacts containing repository context. They are not public benchmark results or full provider usage records.
