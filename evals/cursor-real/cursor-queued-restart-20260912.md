# Real Cursor queued-task restart

Date: 2026-09-12. Result: queued review resumed across an actual server restart with its requirements and job identity preserved.

The isolated PR `majiayu000/harness-prompt-replay-20260912#1` started at `ready_to_merge`, head `543eb465be4b08651fcbd24d1e02e823661721d4`. One native PR submission requested independent inspection of installed 5.x dependencies and Vitest Assertion/Matchers declarations, with real checks and no merge.

Disabling the worker initially caused submission to return HTTP 500: dispatch and worker must both be enabled. This setup failure was preserved. The evaluator instead kept the worker enabled and temporarily set its polling interval to 600 seconds in the isolated project's WORKFLOW.md. The task was accepted and persisted as pending job `c8366414-57ce-4e04-8c1f-46493bb1f4b4` before execution.

The evaluator sent SIGTERM to the task-owned server, restored the original two-second worker interval, and started a new server process against the same isolated database. No resubmission or database mutation was used after restart.

The same job completed successfully through real Cursor thread `1be0cda1-ed6f-4345-950c-ab1cc6959f2c`, with 19 tool calls. Its actual model input retained the submitted requirements. Cursor inspected installed vitest@5.0.0 and coverage-v8@5.0.0, reviewed the declaration augmentation, and ran typecheck, 193 tests and coverage successfully. It reported the exact reviewed head and a clean worktree; Harness returned to `ready_to_merge`.

Before and after restart, the database contained eight total runtime jobs (seven historical completed jobs and this one new job). This job recorded exactly one claim, turn start, prepared prompt, Agent start and result event. No duplicate job or execution was observed. The PR was not merged, business code was not edited by the evaluator, and the evaluation server was stopped after evidence capture. Original WORKFLOW.md contents were restored.

This is a graceful restart of queued work, not an abrupt crash during an active Cursor turn. It does not establish lease-expiry behavior, running-process recovery, cross-host ownership or multi-worker recovery. Cursor used model `auto`; underlying model, tokens and cost are unknown. No Agent replies, workflow outcomes or CI statuses were fabricated.

Evidence directory: `/Users/apple/.local/share/harness/cursor-evals/20260912-prompt-contract/restart-cursor/`. It contains original workflow settings, rejected setup response, native submission, binary digest, stopped-process record, before/after jobs and commands, complete transcript, runtime events and resumed result.
