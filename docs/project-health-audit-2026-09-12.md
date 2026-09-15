# Harness Project Health Audit

> Date: 2026-09-12
> Scope: Current main working tree, including uncommitted changes.
> Status: Revised after source verification and a focused defect-fix pass. Not a deployment report.
> Authorized scope: Fix confirmed defects and update audit status. Skill injection, an eval executor, and budget-policy changes are excluded.

The original audit mixed confirmed defects, incomplete capabilities, and policy choices. This revision replaces those classifications. Existing worktree changes must not be attributed to this fix pass.

## Confirmed defects and current disposition

| ID | Finding | Current disposition |
|---|---|---|
| B1 | Startup constructed an interceptor stack that no production path invoked | Removed the unused AppState/ServicesBundle stack and its private ContractValidator, PostExecutionValidator, and validation-executor dependencies. Runtime activity contracts and reducers retain their existing enforcement. HookEnforcer's public API remains; this does not claim it intercepts live workflow turns. |
| B5 | Merge-readiness LocalReviewPassed did not attest the requested commit | The reducer now requires the pass signal's reviewed_head_sha to equal server-owned merge_review_head_sha and working_tree_clean to be true. Missing, mismatched, or dirty evidence blocks the completion. Prompt instructions explain the required checkout and final checks. |
| B6 | Active-count queries logged list failures and returned incomplete normal counts | Overview, dashboard, and project queue statistics now propagate missing-store and query errors instead of returning partial sums or legacy queue counts. HTTP callers return an error. |
| A-D3 | RuntimeKind selection disagreed across preflight and execution | Existing worktree edits make CodexExec oneshot and CodexJsonrpc per-turn independently of approval policy. This pass additionally passes the selected backend instance into lifecycle so execution does not create another adapter after preflight. Correction retries retain the existing explicit oneshot behavior. |
| A-D9 | Workflow runtime store was optional while legacy TaskStore was critical | Existing worktree edits make workflow runtime storage critical and TaskStore optional. Full TaskStore retirement is not complete. |
| A-D4 | Removed repair cap left MissingBaseline and artificial hygiene blocker bookkeeping | Existing worktree edits remove the hygiene pre-check, MissingBaseline stop, unused round-policy arguments, and associated recovery dialect. Repair rounds remain observable without a three-round limit. |

### Review-evidence boundary

The new B5 check applies to merge-readiness reviews with a server-selected merge_review_head_sha. Initial local review can run without that target, but it does not provide the SHA binding used by the auto-merge gate. Auto-merge still compares the requested head with a fresh GitHub snapshot.

The reviewed SHA and clean-worktree assertion are agent-reported evidence. The reducer verifies their consistency with the server target; it does not independently inspect Git or prove that an agent actually performed the review. Harness continues to delegate Git operations to agent prompts. A trusted worktree attestation mechanism would be a separate capability, not a property of this fix.

## Incomplete capabilities and policy choices

### B2 / D2 — Harness skill injection and Context Composer

Harness's SkillStore discovery exists, but its matching does not feed live runtime packets. ContextComposer serves preview requests while prompt_packet builds live prompts. This is an incomplete integration, not evidence that the underlying Cursor/Codex process cannot load its own skills. No skill-injection or composer architecture change is included in this pass.

### B3 — Eval execution and isolation

Eval jobs use RemoteHost and the local workflow worker intentionally excludes them. That exclusion is correct for remotely owned execution. The audit did not establish whether a suitable external executor is deployed. Unix-process disk limits are unsupported; an operational eval-isolation claim requires executor and resource-enforcement evidence. A Docker executor is a possible implementation, not a verified necessary fix to the local worker.

### B4 — Budget enforcement

The original claim that there is no server-side workflow USD budget mechanism was incomplete. RuntimeBudgetPolicy already provides a default workflow budget of USD 15, an explicit unlimited option, optional daily profile caps, and Shadow/Enforce modes. Dispatcher/completion code implements budget handling. The default enforcement mode is Shadow, so the default records rather than blocks.

Changing that default to Enforce is a product-policy decision, with adapter cost-reporting requirements. No new spending limit or default-mode change is included here. Per-request max_budget_usd=None alone does not establish that no workflow-level budget machinery exists.

### D3 — Sandbox default

DangerFullAccess remains the default. This is a permission-policy choice with adapter/platform constraints, not a defect proven solely by the enum default. No default permission change is included.

### D1 / D4 / D5 — Remaining architecture

TaskStore and additional lifecycle stores still exist. The dual AgentBackend surfaces are intentional; their inconsistent selection was the defect. The presence of multiple stores or modules alone does not prove divergent authoritative writes. Complete retirement and larger ownership changes remain separate work.

## Corrections to the original audit

- **D6 (path-derived production schemas): withdrawn as stated.** Normal server startup constructs shared schemas and ensure_startup_context_not_path_derived rejects path-derived ownership. The helper remains for migration/tests; its existence is not proof that production startup still uses path identities.
- **W1 (removing the three-round repair cap): intentional, not a bug.** The user explicitly requested its removal. It must not be restored under the label of audit remediation. Usage exposure can be discussed separately from a fixed repair-round cap.
- **W2 (broad child-start recovery): narrowed.** The branch requires actor=operator, an explicitly requested local_review_gate target, a GitHub issue workflow, a valid PR number, and non-eval metadata. It does not automatically rerun local review for every child-start failure. No reproduced incorrect recovery was established here.
- **Uncommitted does not mean inactive.** Local builds can run uncommitted code; whether changes are merged is not what determines whether a deployed binary has a defect.
- The original priority list favored wiring old interceptors without checking their semantics. The removed ContractValidator used prompt length and English word checks; automatically inserting it into the live path was not justified.

## Verification

Focused verification completed against this working tree. No full workspace test suite or live production workflow is part of this pass. PostgreSQL checks use a disposable local cluster, never the operational database.

| Command | Result |
|---|---|
| `cargo test -p harness-workflow local_review` | 32 passed |
| `cargo test -p harness-server active_counts_fail_when_runtime_query_fails` | 1 passed; real disposable PostgreSQL fixture |
| `cargo test -p harness-server workflow_runtime_worker::turn_engine` | 20 passed |
| `cargo test -p harness-server workflow_runtime_worker::executor::tests` | 8 passed |
| `cargo test -p harness-server --lib build_app_state_continues_when_optional_task_store_fails` | 1 passed |
| `cargo test -p harness-server --lib build_app_state_aborts_on_critical_workflow_runtime_store_failure` | 1 passed |
| `cargo test --package harness-agents` | 382 unit tests and 1 additional test passed; 7 ignored by the existing suite |
| `cargo test -p harness-server --lib prompt_packet` | 57 passed |
| `cargo fmt --all -- --check` and `git diff --check` | Passed |

Initial check attempts exposed an edit syntax error and newly unreachable private modules; both were corrected before the passing package tests compiled the final code. The first unfiltered invocation of the optional-store startup test was interrupted during compilation, then rerun successfully with --lib. The build reported missing Bun and embedded its existing non-release stub; these results do not verify the web bundle.

Task-start snapshot and verification logs are under `.git/patch-tournament/health-*`. Patch Guard records changes relative to the dirty starting tree; formatting also touched existing edits. Existing store-criticality, recovery, and repair-counter edits predate this pass. No commit, push, production restart, or full workspace test was performed.

## Evidence anchors

- Storage criticality: crates/harness-server/src/http/builders/{storage,registry,registry_failures}.rs and http/init.rs.
- Schema identity: crates/harness-server/src/http/builders/mod.rs and registry.rs.
- Active counts: crates/harness-server/src/handlers/{overview,dashboard_active_counts,dashboard}.rs and http/health_routes.rs.
- Review binding: crates/harness-workflow/src/runtime/reducer/builtin_pr_feedback.rs, runtime/tests/local_review.rs, and crates/harness-server/src/workflow_runtime_worker/prompt_packet/mod.rs.
- Backend identity: workflow_runtime_worker/{runtime_profile,executor/mod,runtime_turn_control,turn_engine/turn_lifecycle}.rs.
- Budget: crates/harness-core/src/config/workflow/budget.rs and crates/harness-workflow/src/runtime/{dispatcher,store/runtime_completion_budget}.rs.
- Recovery eligibility: crates/harness-workflow/src/runtime/store/recovery.rs.

The companion architecture document remains a broader recommendations snapshot; its current-status note takes precedence over its historical findings and roadmap.
