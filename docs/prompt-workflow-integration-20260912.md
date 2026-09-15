# Prompt workflow integration and real Cursor resume validation

Date: 2026-09-12. Status: integrated into the primary checkout; not committed, pushed, or deployed to production.

## Implemented behavior

Built-in prompts now put task context before the output contract and omit redundant scheduler/configuration instructions from the model-facing copy. Durable audit packets and declarative workflow policies remain intact. Exact signal, artifact and validation item fields remain explicit after a real Cursor attempt demonstrated that removing them caused malformed output.

Review findings flow into repair; plans and the most recent repair evidence flow into subsequent review with agent provenance. Repair rounds remain telemetry without a three-round limit. Pending remote readiness does not downgrade successful local work. Cursor no longer receives an unsupported scoped-tool formatting retry after malformed output.

An explicitly cancelled PR can be resubmitted through the native submission endpoint. A narrowly scoped terminal-reopen transition atomically records `PrResubmitted` and queues local review. Passive discovery does not reopen cancellations. Cancellation and previous merge-attempt markers are cleared on explicit resubmission; historical events remain.

The no-check merge policy distinguishes an explicitly absent GitHub rollup from missing snapshot data. The no-check path requires a current-head local approval, complete check facts, no contexts, and CLEAN merge state, plus the existing repository merge policy. Missing, pending, failing or incomplete check evidence does not qualify. Repository-specific requirements such as Harness's passing CI Result still apply.

Integration preserves the primary checkout's independent reviewed-SHA and clean-worktree safeguards. The review example now requests `reviewed_head_sha` and `working_tree_clean`. Trusted merge-head lookup also accepts the server/external-provenance `merge_review_head_sha` field.

## Real execution evidence

Development target: private evaluation PR `majiayu000/harness-cursor-eval-20260910-152946#1`, head `862f99cba2acfd35883a60b90ae3b2f1bfdedc79`.

- Prior successful review job: `40aa78b9-c89f-4b65-bc31-bf4b8909a78a`, with 17 actual tool calls.
- Native cancellation was followed by two concurrent native submissions for the same PR. Both submissions accepted the same workflow. Exactly one `PrResubmitted` event and one new runtime job were persisted.
- Resumed job: `c3b40ac2-3ee5-4f44-a61f-86c97fc023a4`, with 31 actual tool calls. Real Cursor reviewed the PR and reported passing typecheck, 193 tests and coverage. Harness automatically returned to `ready_to_merge`.
- No agent outputs, review findings, CI results or workflow states were fabricated. No direct SQL state mutations were used. No merge was performed. The task-owned evaluation server was stopped after evidence collection.

The resume pilot used the isolated implementation binary before integration of the concurrent SHA/clean-worktree changes. Those integrated protections have focused policy-test coverage but have not yet been exercised by a new real Cursor pilot. This is development-case evidence, not a held-out quality estimate, and it does not prove a new repair/re-review loop or no-check merge execution. Cursor used model `auto`; the underlying model and cost are unknown.

## Verification and attribution

The combined candidate passed `cargo check --workspace --all-targets -j1`, formatting and diff checks. Focused tests passed: local review 20, prompt packet 57, activity status 18, no-check policy 1, absent-rollup boundary 1. These deterministic tests are policy/contract checks, not simulated Agent evaluations. No full-workspace test suite was run locally.

Initial integration exposed obsolete modules from an in-progress concurrent refactor; refreshing from the updated primary checkout resolved those errors. A duplicate test import introduced by merging was then removed. The successful logs supersede these failed integration attempts, which were not counted as passes.

Task changes were three-way merged against the saved task-start patch. Before copying the validated candidate into the primary checkout, SHA-256 guards confirmed that every affected primary-checkout file still matched the integration snapshot. Unrelated changes were preserved. Patch Guard's isolated second-slice report returned `observed`, which records attribution rather than semantic approval.

Evidence directory: `/Users/apple/.local/share/harness/cursor-evals/20260912-prompt-contract/evidence/resume/`. It contains cancellation/submission responses, runtime jobs, workflow events and commands, full review artifacts, binary hash, and validation logs. These local artifacts may contain repository context and should not be published indiscriminately.

## Remaining work

Run the integrated binary through real Cursor review, then the planned admitted and held-out cases, including genuine findings leading to repair/re-review, restart continuity and remote merge boundaries. Only after those checks should production rollout expand. The complete evaluation plan remains unfinished; this report does not establish that all issues and PRs can now complete autonomously.
