# Prompt and Workflow Improvement Plan

Status: Proposed; implementation and production changes are not part of this planning task.
Date: 2026-09-12

## Objective and boundaries

Improve autonomous issue resolution, PR repair, independent review, and merge completion through Harness-operated Cursor. Reduce contradictory instructions, lost handoffs, and avoidable orchestration stops. Prompt length is a diagnostic measurement, not an optimization target or acceptance gate.

Keep the existing workflow runtime, activity boundaries, independent reviewer processes, event history, repository requirements, and real GitHub verification. Agents choose implementation and relevant validation methods. Harness owns dispatch and lifecycle decisions for the built-in issue/PR workflow.

This plan does not introduce a prompt compiler, another workflow engine, a generic policy DSL, new production scoring rules, compatibility layers, or a new memory service. Do not reinstate the removed three-round repair limit. Do not modify business issues/PRs directly from the supervising session; use Cursor through Harness. Operator merge approvals remain distinct from autonomous agent completion.

This is the immediate, bounded plan for the observed Cursor workflow. The broader proposals in `docs/prompt-workflow-contract-long-term-design.md` are background, not prerequisites for these changes.

## Reference comparison: OpenAI Symphony

Inspected upstream revision: `e0ccc83720a42a600a53b61c5f8d3e518bebe1db`.

| Measurement | Result | Boundary |
| --- | ---: | --- |
| `elixir/WORKFLOW.md` | 19,202 characters; 329 lines | Configuration plus template |
| Markdown prompt body | 18,252 characters; 288 lines | YAML removed; body trimmed; Liquid not rendered |
| `elixir/AGENTS.md` | 3,118 characters | Additional repository instructions; actual loading depends on agent behavior |
| Same-run continuation guidance | Approximately 0.53k characters | Example continuation numbers; excludes existing thread history |
| `SPEC.md` | 92,077 characters | Service specification, not the ordinary runtime prompt |

These are character measurements, not token counts. The rendered first prompt depends on the issue description and attempt conditional. Skills, tool schemas, agent instructions, and provider context add further input. No equal-token or equal-cost comparison is established by these measurements.

Verified from the reference implementation:

- `PromptBuilder` renders the workflow body with issue fields and attempt context. It does not append the Harness-style activity-result/state-machine packet.
- `AgentRunner` opens an app-server session, reuses its thread for continuation turns, and checks refreshed tracker state after each turn. Later turns contain continuation guidance instead of restating the full initial prompt.
- The example workflow keeps a persistent workpad containing plan, acceptance, validation, notes, and confusions. This is a working record, not a replacement for repository or tracker truth.
- The example delegates ticket updates to the agent and includes human-review and merging states. It also specifies a full reset for rework. Those are example policies, not requirements to transplant into Harness.
- Symphony still has per-run turn limits and scheduling policies. Thread reuse is not evidence of unlimited uninterrupted execution or higher repair quality.

Decision: adapt readable task guidance, durable handoffs, reproduction and acceptance evidence, and purposeful continuation. Keep Harness's server-owned built-in transitions and independent reviewers. Do not adopt mandatory Linear comments, full PR replacement on rework, or a platform rewrite. Do not assume Cursor supports the same session semantics without a separate capability check and real experiment.

Sources:

- [Workflow example](https://github.com/openai/symphony/blob/e0ccc83720a42a600a53b61c5f8d3e518bebe1db/elixir/WORKFLOW.md)
- [Prompt builder](https://github.com/openai/symphony/blob/e0ccc83720a42a600a53b61c5f8d3e518bebe1db/elixir/lib/symphony_elixir/prompt_builder.ex)
- [Agent runner](https://github.com/openai/symphony/blob/e0ccc83720a42a600a53b61c5f8d3e518bebe1db/elixir/lib/symphony_elixir/agent_runner.ex)
- [App-server session implementation](https://github.com/openai/symphony/blob/e0ccc83720a42a600a53b61c5f8d3e518bebe1db/elixir/lib/symphony_elixir/codex/app_server.ex)
- [Service specification](https://github.com/openai/symphony/blob/e0ccc83720a42a600a53b61c5f8d3e518bebe1db/SPEC.md)

## Harness evidence and limits

Read-only inspection covered the current checkout, production prompt events, transcript artifacts, and completed jobs. Production samples change as jobs complete; freeze IDs and digests before implementation experiments.

- One sample of 100 actual transcripts contained 2,184 tool-call items and 612 agent-reasoning items. These counts show substantial tool activity, not its correctness or usefulness.
- The same transcript sample had a Harness user-input median of 23,790 characters and maximum of 40,207. This is not the full provider context or measured token consumption.
- A sample of 150 prepared packets had an `activity_result_schema` serialized median of approximately 9,980 characters. Packet audit fields are stripped before rendering; stored packet size must not be equated with model input size.
- No packet in that sample contained repository memory; all workflow prompt bodies were empty. The production repository-memory table contained zero rows, and sampled execution artifacts reported memory disabled.
- A follow-up sample contained 20 repair packets with `review_summary`, but no directly supplied `local_review_findings` or previous-attempt history. Fifty local-review packets had no directly supplied issue plan. Repository files may still supply additional context.
- Implementation does receive `issue_plan` and its summary through the untrusted command-data section. The handoff is not entirely absent.
- A real merge packet said no reducer transition existed and success left state unchanged, while its summary contract required a verified merged PR. The merge activity falls through the default transition description.
- Vibeguard #801 included valid repairs, repeated formatting failures, and repair results downgraded because of GitHub merge status. Its sixth repair round and eventual merge show why round count alone was an inadequate stop criterion.

These observations establish concrete contract and handoff problems. They do not prove that prompt length causes poor performance, that all repeated reviews are unnecessary, or that repository memory would improve outcomes if enabled unchanged.

## Findings and decisions

| Priority | Finding | Required change | Preserve |
| --- | --- | --- | --- |
| P0 | Built-in agents are told to report facts and also generate lifecycle commands | Remove optional workflow decisions and command examples from built-in model-facing instructions after checking actual reducer consumers; keep transitions server-owned | Custom declarative workflows that explicitly need agent decisions |
| P0 | Activity guidance and reducer behavior can disagree | Make the existing activity contract authoritative for model-facing requirements; correct merge guidance and stage-specific examples | Strict parsing of the minimal result envelope and explicit errors |
| P0 | Action completion, code correctness, and remote readiness are conflated | Route by activity outcome plus typed facts; do not infer failed repair from CI waiting or prose | Real failures, unresolved findings, repository protections |
| P0 | Review-to-repair handoff is primarily a summary | Pass actual findings, reviewed revision, relevant evidence, and prior dispositions through existing artifacts/commands | Independent review and untrusted-data boundaries |
| P1 | Plans describe implementation but do not reliably preserve acceptance intent | Ask for current/expected behavior, scope, observable acceptance, and chosen validation approach | Agent freedom over methods; no mandatory standalone spec for small tasks |
| P1 | Full runtime configuration and protocol instructions obscure task content | Render only relevant task and activity context; retain full packets for audit | Necessary background regardless of character count |
| P1 | Continuation lacks a concise durable account of attempted work | Reuse existing task artifacts to render a current handoff; use references for older evidence | Fresh reviewer processes and immutable history |
| P1 | Baseline-count absence can stop repair even after count limits were removed | Remove count completeness as a business-progress gate; classify missing actionable evidence separately | Round counts as telemetry and existing infrastructure controls |
| P1 | No-check repositories and missing merge SHA cause non-prompt stalls | Distinguish no required checks from unavailable/unfinished checks; align reviewed/current/expected head usage | SHA-bound approval, repository CI rules, squash and post-merge verification |
| P1 | Cancelled items are treated as covered but explicit PR resubmission cannot resume them | Preserve cancellation for unattended intake; allow explicit authorized resubmission with audited identity/ownership | Dedupe, freezes, and rejection of concurrent duplicate owners |
| P2 | Repository memory is disabled and session continuation is unproven for Cursor | Evaluate separately after task handoffs work | Isolation between experiments and reviewer independence |

## Intended prompt organization

Keep one existing renderer with activity-specific content. Do not add a new abstraction layer to express these sections.

1. **Task:** issue/PR identity, requested behavior, original acceptance requirements, scope, and applicable authorization.
2. **Current state:** phase, branch/head/base evidence and timestamp, real review findings, checks where relevant, and unresolved dependencies.
3. **This activity:** a plain-language objective, completion meaning, and any required side effects such as pushing changes or verifying the merge.
4. **Handoff:** what has already been done, evidence at the relevant revision, unresolved findings, and failed approaches worth retaining.
5. **Result:** one transport-appropriate envelope and one realistic example for this activity. Free-form evidence remains free-form; the server handles event IDs, dedupe keys, command dispatch, and database state.

Full original issue requirements must remain accessible. Do not truncate them to meet a target size. Exclude internal leases, dispatch claims, irrelevant workflow settings, and custom-workflow commands from ordinary built-in prompts. Preserve external text as untrusted evidence without repeating wrapper explanations around unrelated scheduler metadata. Never promote issue comments or prior agent opinions to instructions merely because they are useful context.

| Activity | Agent work | Completion handed to Harness |
| --- | --- | --- |
| Plan | Understand/reproduce the issue as feasible; identify scope, acceptance, and approach | A useful plan or a concrete reason the task cannot proceed; unknown files remain explicit unknowns |
| Implement | Apply a bounded solution, choose appropriate verification, push and create/update the PR | Changed behavior, validation evidence, PR binding, remaining concerns |
| Review | Independently compare the current patch with requirements; check prior findings and changed risk areas | Explicit outcome, reviewed SHA, concrete findings with evidence; no unearned approval |
| Repair | Address remaining findings and required branch/check repairs; verify thread dispositions | Action evidence, resulting SHA, validation, what remains unresolved |
| Merge | Verify current head, authorization and repository conditions; squash merge when eligible | Verified remote merged state or the exact unmet condition |

Missing tools/access, actual failing checks, pending checks, no configured required checks, and unknown check availability are different facts. Avoid a new global taxonomy or heuristic phrase dictionary: use existing activity outcomes and GitHub facts at their established boundaries.

## Implementation sequence

### 0. Freeze a usable baseline

Record the exact Harness revision plus uncommitted diff, deployed binary identity, rendered input/digest, workflow configuration, Cursor CLI version, requested model and source commit. Preserve the current dirty checkout and production tasks. Select representative completed real runs including #801, ordinary issue work, no-change cases, and merge waits. Reuse `evals/cursor-real/` evidence conventions.

Acceptance: baseline cases can be replayed from identical isolated starts; no missing input, model uncertainty, or operator intervention is silently omitted.

### 1. Correct and simplify the activity contract

Primary surfaces: `workflow_runtime_worker/prompt_packet/mod.rs`, `activity_contract.rs`, and the relevant built-in reducers.

- Inventory each consumer of agent-proposed decisions before removing built-in instructions for them. Keep custom declarative behavior scoped to its declared contract.
- Replace generic workflow-decision examples with the current activity's result. Remove transport alternatives from Cursor input when they do not apply.
- Generate visible required fields, accepted outcomes, and examples from existing contract definitions where practical; do not create a compiler or new configuration surface.
- Add the real merge completion description. Align planning's explicit-unknown guidance with accepted plan content.
- Render meaningful task instructions before protocol details. Keep full audit packets separately; do not claim the audit data is all model-visible.

Acceptance: each native activity has one coherent instruction/result contract; a real Cursor can complete it without inventing scheduler commands. Format-only failures are recorded separately from task failures.

### 2. Repair outcome interpretation and waiting

Primary surfaces: `activity_status_contract.rs`, completion reducers, repair-convergence policy, `http/auto_merge.rs`, merge preparation and completion verification.

- Separate successful repair from whether all remote gates have finished. A pushed, locally verified repair waiting for CI proceeds to review/waiting as appropriate.
- Preserve explicit review failures and actual execution errors. Do not obtain higher success rates by accepting every succeeded claim.
- Remove missing blocker-count baseline as an autonomous repair stop. If the next action lacks necessary evidence, refresh the evidence or report the actual missing dependency.
- Share existing server-observed head facts between automatic and operator merge paths. Recheck current GitHub head before mutation; reject stale approval.
- For repositories with no required checks, use independent validation and applicable repository policy. A missing API response or unfinished required check is never equivalent to no checks required.
- Keep GitHub reconciliation authoritative after a merge, including merges initiated by an operator.

Acceptance: real waiting-CI and no-check cases do not become false code failures; failing required checks still prevent merge; changed heads require appropriate renewed review; merged state is independently verified.

### 3. Preserve requirements and repair handoffs

Primary surfaces: `runtime/pr_feedback.rs`, `builtin_plan_issue.rs`, prompt assembly and existing workflow artifacts.

- Enrich plan guidance with expected behavior and acceptance examples without adding mandatory documents or fixed commands.
- Pass the plan and relevant original requirements into review, not only implementation.
- Pass actual review findings into repair, with reviewed SHA, evidence and disposition. Reference existing artifact IDs rather than introducing a new storage system.
- Render a compact current working record from existing task artifacts: completed work, current revision, open findings, failed approaches and validation evidence. Preserve older evidence by reference.
- Tie dispositions to a finding and the revision that addressed it. A reviewer must be free to reject an author's claim and to identify new regressions.
- Do not blindly carry old approvals or rejected findings across changed code. Show evidence, not a suggestion that the reviewer should approve.

Acceptance: real repair can identify what remains without reconstructing the entire prior run; review can check original acceptance; no valid finding disappears through summary compression or stale-memory reuse.

### 4. Verify continuation and resume coverage

Primary surfaces: existing recovery/submission/intake paths, especially PR resubmission and coverage selection.

- Allow explicit authorized resubmission of a cancelled PR, consistent with issue resubmission, with one current workflow owner and retained cancellation history.
- Keep unattended intake from reviving intentional cancellations or frozen tasks automatically.
- Check restart recovery of task handoff and command ownership with real processes and an isolated database.
- Ensure pending CI, external blockers, queue waiting, local review, and merge waiting are visible as different reasons to the operator.

Acceptance: no dedicated clone or direct SQL mutation is needed to resume an explicitly authorized cancelled PR; no duplicate execution is introduced; restart preserves unfinished work and its evidence.

### 5. Evaluate before enabling broader memory or session reuse

Do not couple this step to the first rollout. First measure whether improved task handoffs remove repeated work. If material repetition remains, inspect actual Cursor continuation capabilities and compare fresh-process handoffs with supported same-author session continuation in isolated runs.

Independent review always starts in a fresh process/context. A continuation prompt must not claim earlier context exists unless the same session actually retains it. Cross-repository memory is out of scope. Repository memory, if tested, contains verified scoped evidence and uses separate stores per variant; no automatic promotion of agent conclusions to policy.

Acceptance: measured quality or continuity benefit without missed findings or contamination. If no benefit is established, retain fresh processes with task handoffs and leave repository memory disabled.

## Real evaluation plan

Reuse `evals/cursor-real/catalog.json` and its current 15 development cases. They are not held-out evidence and several require admission/calibration before use. Add a small held-out set of newly selected real tasks before tuning; keep task-time inputs and evaluator acceptance separate from solution history.

Coverage must include:

- Simple, medium and hard issue repairs; clean/no-change tasks to detect unnecessary edits.
- A real review finding followed by repair and independent re-review, only when a real reviewer actually finds a defect.
- Multiple findings, a failed prior approach, and newly introduced regressions.
- CI pending then successful; genuinely failing CI; a repository with no required CI; unavailable check data.
- A stale PR head, merge conflict, verified merge and external merge reconciliation.
- Explicit cancelled-task resubmission, restart continuity and concurrent work across repositories.
- A real external dependency or maintainer freeze that should remain blocked.

Protocol:

1. Calibrate each admitted evaluator against the broken start and a known-good revision or independently reviewed acceptance basis. Use real processes/services and disposable databases where needed.
2. Compare baseline to phase 1, then add outcome handling, then handoff changes. Change one main factor at a time and use identical starts. An initial pilot is diagnostic; rerun the full admitted suite for the combined candidate.
3. Run every candidate through `POST /api/workflows/runtime/submissions` with real Cursor. Do not fake model replies, tool events, review verdicts, CI statuses, or a rejected review to force a repair loop.
4. Hold CLI version, requested model, configuration and resource ceilings constant. If `auto` remains necessary, report underlying-model uncertainty rather than claiming a controlled model comparison.
5. Randomize/alternate variant order. Keep workspaces, databases, memory and private evaluator materials separate. Prevent post-resolution comments and reference patches from leaking into initial task inputs.
6. Repeat selected cases with new actual executions. Use matched concurrency for performance comparisons; queue wait and tool/CI wait are separate from agent execution time.
7. Evaluate final behavior with independent real checks and a reviewer outside the tested repair loop. Do not use the workflow's own approved flag as the sole correctness criterion.

Metrics and decisions:

| Metric | Interpretation |
| --- | --- |
| Independently resolved tasks / all scheduled tasks | Operational autonomous completion, including infrastructure failures in the denominator |
| Independently resolved tasks / evaluable attempts | Quality with excluded/incomplete environments separately disclosed |
| False completion and missed valid findings | Must not be hidden by a reduced prompt or relaxed parser |
| Operator interventions | Manual state recovery, manual merges and manual code edits are separately attributed |
| Handoff omissions and repeated failed approaches | Transcript-backed cases, not a guessed count of similar tool calls |
| Paired improvement/regression counts and repeated-run success | Report denominators and uncertainty; one win on 15 tasks is not reliable evidence |
| Elapsed time, queue/CI time, observed usage | Efficiency only among comparable-quality results; unknown spend remains unknown |
| Per-section characters and protocol corrections | Diagnostics, not the primary objective or a hard prompt-size budget |

A candidate advances only after investigating paired regressions, with no observed unauthorized merges, fabricated evidence, or reproducible acceptance regressions. Report this as bounded evidence, not proof of universal correctness. Do not adopt arbitrary aggregate percentage thresholds or hide individual failures in a weighted score.

## Validation, rollout and completion

Use focused repository checks: affected-package `cargo check --all-targets`, the relevant deterministic policy tests, `cargo fmt --all -- --check`, and workspace Clippy before pushing a PR. Do not run `cargo test --workspace` locally. PostgreSQL checks use isolated disposable databases and narrow filters. Deterministic contract checks do not stand in for real agent evaluation.

Keep changes in reviewable slices: activity contract/rendering, outcome/merge semantics, task handoffs, then recovery consistency. Implement Harness changes directly; all business repair and real agent evaluations remain Harness-operated Cursor work. No business repository gets Harness-specific validators or bookkeeping files solely for this project.

Roll out after isolated real evaluation: a small production canary across different task types, then existing configured concurrency, then verified 20-process cross-repository coverage if capacity permits. Count actual Cursor processes separately from reserved runtime jobs. Change future dispatch only; drain in-flight work before a binary change when necessary. Record deployed binary and prompt digests.

Rollback changes the deployment/configuration for future dispatch and preserves completed commits, remote merges and event history. It must not blindly restore old database snapshots or replay already-completed mutations. The operator-authorized removal of the three-round limit remains part of the deployment baseline.

Complete when native plan/implement/review/repair/merge inputs are coherent, real handoffs preserve requirements and findings, the admitted real suite and held-out results are reported, merge/resume edge cases behave correctly, and production evidence shows the intended behavior without supervisor code fixes or hidden manual recovery. Do not call the whole plan complete merely because a prompt became shorter or one PR merged.
