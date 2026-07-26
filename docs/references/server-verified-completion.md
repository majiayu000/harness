# Server-Verified Completion: The Trust Boundary of "Done"

> Status: analysis reference for GH-1766
> Date: 2026-07-26
> Scope: how the workflow runtime decides an activity or workflow succeeded, which
> parts of that decision are server-verified facts, and which parts are the
> agent's own claims. All citations verified against `main` at the time of
> writing.

## Summary

The runtime has a strong *format* contract for completion — a missing or
malformed structured result fails the activity, zero-output spawns are
classified as failures, and self-contradictory success claims are downgraded.
But the *content* of a successful result is still almost entirely
agent-authored. The server-side evidence machinery that could turn "done" into
a verified fact exists (`TransitionRule::required_evidence`, evidence classes,
reducer contract checks) and is enforced for declarative YAML workflows, while
all four built-in workflow definitions run with empty evidence requirements.
Quality-gate validation commands are executed by an LLM, never re-run by the
server. A PR number is bound to a workflow straight from an agent artifact
with no existence check. A prompt task reaches `done` on nothing more than the
agent's own `succeeded` status.

Every downstream consumer — skill-quality EMAs, GC signals, eval baselines,
operator dashboards, GitHub coverage accounting — inherits whatever fiction
survives this boundary.

## 1. What already works (and must be preserved)

### 1.1 Zero-output detection

`crates/harness-server/src/workflow_runtime_worker/activity_result.rs`
counts assistant messages, tool invocations, and structured result blocks
(`agent_activity_summary` / `is_zero_output`). A turn that completes with no
observable work is classified as
`ActivityResultEnvelopeOutcome::ZeroOutputSpawnFailure`, logged at error
level, and surfaced as an `AgentZeroOutputSpawnFailure` signal. An exit-0
no-op spawn cannot become `succeeded`. This closed the worst historical
false-done class
(zero-output spawns counted as done during the 2026-07 retry storms).

### 1.2 Structured result required, fail-closed

A `Completed` turn without a fenced `harness-activity-result` JSON block is
`MissingStructuredOutput` and maps to a failed activity, explicitly
to prevent silent state-machine no-progress loops. Invalid JSON or a
mismatched activity name also fails.

### 1.3 Self-contradiction downgrade

`workflow_runtime_worker/activity_status_contract.rs::enforce_activity_status_contract`
rewrites `succeeded` to `blocked` when the agent's own payload
reports blockers: blocking signals (`ChangesRequested`, `ChecksFailed`,
`QualityFailed`, …), structured fields (`failing_checks`,
`unresolved_review_threads`, adverse `merge_state_status`), or textual
blockers in the summary.

### 1.4 Server-owned PR inspection and merge gate

`inspect_pr_feedback` is a server-owned activity: the server itself queries
GitHub GraphQL and builds the snapshot; no LLM is involved. PR readiness
demands `snapshot_source == "server_github_graphql"` with matching PR
identity, head, and thread/check state
(`runtime/reducer/pr_feedback_completion.rs`, `ready_snapshot_proves_pr_ready`).
The auto-merge gate is deterministic over a server-fetched snapshot and
harness never calls the GitHub review-approval API.

### 1.5 Piecemeal reducer contract checks

`pr_feedback_completion.rs::pr_feedback_success_contract_error`
rejects an `address_pr_feedback` success that lacks a `pr_repair_snapshot`
proving pushed changes, review-thread action, or an explicit no-code-change
reason plus validation. This is the model the rest of the system
should follow — but it is hand-built for one activity, not a general
mechanism.

### 1.6 Terminal-evidence validation for hidden transitions

`runtime/validator_github_issue_pr.rs::validate_reconciliation_only_done`
restricts hidden `→ done` transitions to the `reconciliation` actor
and requires `pr_number` plus `github_pr`/`github_issue` evidence. Reconciliation itself maps real GitHub facts (`PrMerged`,
`IssueCompleted`) to terminal states independently of any agent claim.

## 2. The false-done risks

### Risk A — prompt tasks reach `done` on the agent's word alone

`crates/harness-workflow/src/runtime/reducer/prompt_task_completion.rs`:
with no continuation policy configured, a `succeeded` `implement_prompt`
activity immediately produces `single_shot_done_decision` — a `MarkDone` command with only
`runtime_completion_evidence` (a record *of the agent's result*, not an
independent fact). The activity contract for `implement_prompt` nominally
requires `validation_evidence`
(`workflow_runtime_worker/activity_contract.rs:143`), but `success_requires`
is consumed **only** by `prompt_packet.rs` — it is prompt text shown to the
agent, not a server-side check. Nothing verifies a diff exists, a validation
command ran, or a `validation_report` artifact is present.

Consequence: an agent that writes a paragraph and emits
`status: succeeded` completes a prompt workflow. The legacy
`task_executor` path that had the same defect (`outcome.rs` marking Done on
non-empty text) was deleted in #1706, but the live runtime path reproduces
the same trust shape.

### Risk B — the structured result is agent-authored, with no independent re-verification

Status, artifacts, signals, and validation records in the
`harness-activity-result` block are all written by the agent
(`activity_result.rs` scrapes the fenced block from agent output). The only
counterweight, `enforce_activity_status_contract`, catches
*self-contradiction*; a confident, internally consistent fabrication passes.
There is no server-side re-execution of any claimed validation for
`implement_issue` or `implement_prompt`.

### Risk C — `BindPr` trusts an agent artifact at bind time

`runtime/reducer/github_issue_completion.rs::bind_pr_from_activity_result`
reads `pr_number`/`pr_url` straight from the agent artifact via
`pull_request_artifact(result)` and issues
`WorkflowCommand::bind_pr`. No GraphQL existence check, no verification that
the PR head belongs to the workflow's branch, no repo match. A fabricated or
mistyped PR number moves the workflow to `pr_open`, and server verification
arrives only later (inspection / merge), meaning intermediate states,
dashboards, and coverage accounting operate on an unverified binding.

### Risk D — `required_evidence` is empty for every built-in workflow

`runtime/validator.rs` defines `TransitionRule::required_evidence`, but both
rule constructors default it to empty, and all four built-in allowlists
(`github_issue_pr_defaults`, `quality_gate_defaults`, `pr_feedback_defaults`,
`prompt_task_defaults`) build rules exclusively via `.allow(...)`.
`implementing → done` is an allowed transition gated on nothing but the
command being `MarkDone`.

Importantly, the *enforcement* path is already general: every decision
transition passes through
`validator_progress::validate_declarative_transition_metadata` (called from
`DecisionValidator::validate_decision` in `validator.rs`), which rejects a
decision missing any kind in the rule's `required_evidence` set — regardless
of whether the definition is built-in or declarative. (The similarly named
`declarative_decision_missing_required_evidence` in
`store/runtime_completion.rs` is an additional declarative-only pre-check,
not the enforcement site.) The gap is therefore purely one of population:
the only site that ever fills `required_evidence` is `declarative.rs:526`
(YAML policy files). The shipped workflows that do all the real work run
with empty sets, so the machinery protects optional declarative workflows
while giving false assurance about the built-ins. One nuance to preserve:
the check deliberately exempts same-state `retry_failed_runtime_activity`
decisions, so evidence requirements bind terminal transitions, not retries.

### Risk E — quality gate is an LLM reporting on itself

`runtime/quality_gate.rs` (62 lines) only *builds the decision* to enqueue a
`run_quality_gate` activity carrying `validation_commands`. The
activity is executed by an agent under the output contract
`quality_gate_status_signal_and_validation_evidence`
(`activity_contract.rs:153-155`) — i.e. the "independent" gate is another LLM
emitting `QualityPassed` plus a self-authored `validation_report`. The server
never re-executes the configured commands to confirm the claimed evidence.
(The legacy `validation_gate.rs`, which soft-passed when no test command was
detectable, was removed with the legacy task layer; the surviving defect is
this agent-executed gate.)

### Risk F — advisory quality grading scores a do-nothing window as 100/A

`crates/harness-observe/src/quality.rs::grade`: with zero events,
`total = events.len().max(1)`, so security/stability/coverage/performance all
compute to 100 and the weighted grade is A. Mitigated by being advisory (it
only tunes GC trigger frequency), but it is a misleading surface on
dashboards and any future consumer that treats Grade A as "healthy" inherits
the bias.

## 3. Remediation design (priority order)

### R1 — Populate `required_evidence` on built-in critical transitions

Because enforcement is already live in the shared validation path (Risk D),
populating the sets in the built-in default constructors activates it
directly — no enforcement-path changes are needed. The remaining work beyond
population is defining the evidence-kind constants, building the producers
that mint them server-side (R2–R4), and tests. Attach evidence classes to
the transitions that mint user-visible facts:

| Workflow | Transition | Required evidence class |
| --- | --- | --- |
| `github_issue_pr` | `* → done` (non-reconciliation) | `github_pr` (verified PR identity) + `server_pr_snapshot` |
| `github_issue_pr` | `implementing → pr_open` | `verified_pr_binding` (see R3) |
| `prompt_task` | `implementing → done` | `prompt_completion_evidence` (umbrella kind, see R4) |
| `quality_gate` | `checking → passed` | `server_validation_digest` (see R2) |
| `pr_feedback` | `inspecting → ready_to_merge` | `server_pr_snapshot` (already produced; make it structurally required) |

`required_evidence` is a conjunctive set — every listed kind must be present.
Where the product rule is disjunctive ("validation report OR explicit
no-change rationale"), the reducer checks the concrete alternatives and mints
a single umbrella evidence kind that the transition requires (R4), keeping
the validator check simple and conjunctive. Fail closed: missing evidence
blocks the decision with a typed reason code
(`MissingRequiredEvidence`); it must never downgrade to a warning.

### R2 — Server-side re-execution of quality-gate validation

The server (not the agent) runs the configured `validation_commands` in the
workflow's workspace, records exit codes plus an output digest
(command, cwd, exit, sha256 of captured output, duration), and attaches the
digest as `server_validation_digest` evidence. The agent-executed activity
remains as triage/diagnosis input; its `QualityPassed` signal alone no longer
satisfies the gate. Timeouts and non-zero exits map to `QualityFailed` with
the digest attached, so operators can distinguish "validation failed" from
"validation never ran".

### R3 — Verify PR binding at `BindPr`

Before applying `WorkflowCommand::bind_pr`, the server resolves the claimed
PR via the existing GraphQL client: it must exist, be open, target the
expected repo, and have a head ref/branch consistent with the workflow's
workspace branch. Success attaches `verified_pr_binding` evidence (pr number,
head oid, observed time); failure produces a typed blocked decision rather
than `pr_open`. This converts Risk C's delay-shaped hole into a gate.

### R4 — Prompt-task done contract

`single_shot_done_decision` and `settled_done_decision` require either a
`validation_report` artifact (server-checkable: command list + exit codes) or
an explicit structured `no_change_rationale`. When either is present, the
reducer attaches the umbrella `prompt_completion_evidence` kind (recording
which alternative satisfied it), and the transition's `required_evidence`
names only that umbrella kind — encoding the OR in the reducer, where the
branching context lives, rather than extending `TransitionRule` with
alternative-set semantics. Absent both, the decision is `blocked` with reason
`prompt_completion_evidence_missing`. The continuation path keeps its
external-state semantics; only the terminal step gains an evidence
requirement.

### R5 — Truthful empty-window grading (small, independent)

`QualityGrader::grade` should return an explicit `Grade::Unknown` (or skip
emission) for an empty event window instead of manufacturing 100/A.

## 4. What this deliberately does not change

- The agent-authored result block stays — as *input* to decisions, never as
  the verdict.
- No new workflow states and no changes to the state graphs; evidence
  attaches to existing transitions.
- Declarative YAML workflow semantics are unchanged (they already enforce
  evidence); the built-ins are brought up to the same bar.
- Reconciliation's authority over terminal states is preserved — it is the
  model: server-observed GitHub facts outrank agent claims.

## 5. Verification sketch

- Table tests per built-in definition proving each critical transition
  rejects a decision lacking its evidence class and accepts one carrying it.
- A quality-gate integration test where the agent claims `QualityPassed` but
  the server re-run fails → workflow blocks with the digest recorded.
- A `BindPr` test with a nonexistent PR number → typed blocked decision, no
  `pr_open` transition, no coverage claim.
- A prompt-task test where the agent returns prose + `succeeded` and no
  validation artifact → blocked, not done.
- Regression tests proving reconciliation-driven done paths still work
  unchanged.
