# Native PR requirements and ready-state resubmission

Date: 2026-09-12. Status: implemented and verified locally; not committed, pushed, or deployed to production.

## Changes

Native PR submissions now pass their additional prompt into local review admission and persist it in `additional_prompt` with external provenance. Subsequent activities receive it through existing workflow data. A new supplied prompt replaces that field; submissions without a prompt preserve it. No new configuration or storage layer was added.

Explicit submissions may request local review from `ready_to_merge`. Issue-bound PRs with new requirements use local review instead of remote feedback inspection. Admission, state changes, persisted requirements and the new command remain in the existing atomic transition. Starting renewed review clears prior merge-head/attempt markers. Automatic discovery and terminal cancellation policies are unchanged.

## Verification

- A focused PostgreSQL test used disposable database `test_cursor_requirements_20260912`. It covered new-PR requirements, denied admission leaving existing requirements/state/commands untouched, and direct ready-state review admission. Passed. This is a persistence test, not simulated Agent output.
- `cargo check -p harness-server --all-targets -j1`, the CLI binary build, formatting and diff checks passed.
- An initial test compile failed because an existing concurrency-test call lacked the new optional argument; fixed. A subsequent test attempt refused a database name outside the project's allowed test naming convention; a fresh correctly named disposable database was used without overriding the guard.

## Real Cursor execution

The existing isolated PR `majiayu000/harness-prompt-replay-20260912#1` was already at `ready_to_merge`, head `543eb465be4b08651fcbd24d1e02e823661721d4`.

One native submission supplied this new requirement only through the request prompt: inspect actual installed Vitest and coverage-provider versions after frozen-lockfile installation and report both resolved versions. The evaluation WORKFLOW.md was not changed for this request.

The endpoint returned HTTP 202 and `local_review_gate` without cancellation or direct state mutation. Actual model input included the new requirement. Real Cursor job `fb7a92df-bbee-4644-963b-f8b2771b6f18`, thread `9bd2e1a6-e41c-4467-9bef-2328cceeaa84`, made seven tool calls and reported both dependencies resolved to 5.0.0. Typecheck, 193 tests and coverage passed; review reported the current SHA and clean worktree. Harness automatically returned to `ready_to_merge`.

No business code was changed, no PR merged, and no Agent output was fabricated. The task-owned evaluation server was stopped after evidence capture. Model selection was `auto`; underlying model and cost are unknown.

Evidence: `/Users/apple/.local/share/harness/cursor-evals/20260912-prompt-contract/pr-requirements/`, including submission, binary hash, full transcript, prepared packets, durable state/events/commands and validation logs.

## Remaining scope

This closes the two product gaps identified by the preceding replay: lost additional PR requirements and direct review admission from the ready state. It does not complete the development/held-out suite, restart-recovery testing, merge-boundary testing or production rollout. Requirements received while an activity is already active remain subject to existing duplicate/admission behavior; this change does not add mid-turn instruction delivery.
