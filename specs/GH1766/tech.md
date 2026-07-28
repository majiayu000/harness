# Tech Spec

## Linked Issue

GH-1766

## Design Overview

Four fail-closed changes, all reusing existing machinery:

1. **Evidence population** — populate
   `TransitionRule::required_evidence` for the built-in definitions; the
   shared decision-validation path already enforces the field for all
   definition kinds, so population alone activates it.
2. **Server-side quality-gate validation** — the runtime worker executes the
   configured `validation_commands` itself and attaches a
   `server_validation_digest` evidence record; the reducer requires it for
   `checking → passed`.
3. **Verified PR binding** — resolve the claimed PR through the existing
   GitHub client before applying `WorkflowCommand::bind_pr`.
4. **Prompt-task completion evidence** — `single_shot_done_decision` /
   `settled_done_decision` demand a `validation_report` artifact or a
   structured `no_change_rationale`.

The agent-authored `harness-activity-result` block remains the transport for
claims; every change here converts one class of claim into a server-checked
fact before the claim can mint a transition.

## Current State (verified citations)

| Fact | Location |
| --- | --- |
| `required_evidence` field exists, defaults empty | `crates/harness-workflow/src/runtime/validator.rs:24,39,53` |
| Built-in allowlists never populate it | `validator.rs`: `github_issue_pr_defaults`, `quality_gate_defaults`, `pr_feedback_defaults`, `prompt_task_defaults`; `implementing → done` via bare `.allow("implementing", "done", [MarkDone])` in the `github_issue_pr` and `prompt_task` defaults |
| Only declarative workflows populate evidence | `crates/harness-workflow/src/runtime/declarative.rs:526` |
| Enforcement is already general: every decision transition runs the `required_evidence` check for all definition kinds | `validator.rs::validate_decision` → `validator_progress::validate_declarative_transition_metadata` (rejects with `MissingRequiredEvidence`; exempts same-state `retry_failed_runtime_activity`) |
| A separate declarative-only pre-check also exists | `crates/harness-workflow/src/runtime/store/runtime_completion.rs` (`declarative_decision_missing_required_evidence`) |
| `required_evidence` is a conjunctive `BTreeSet` — no OR semantics | `validator_progress.rs` (missing-kind filter over the set) |
| Quality gate builds an enqueue decision only | `crates/harness-workflow/src/runtime/quality_gate.rs` (62 lines) |
| Gate contract is prompt prose, not enforcement | `crates/harness-server/src/workflow_runtime_worker/activity_contract.rs:25,63,153`; `success_requires` consumed only by `prompt_packet.rs` |
| `BindPr` reads the agent artifact unverified | `crates/harness-workflow/src/runtime/reducer/github_issue_completion.rs:177,193` |
| Prompt task done on agent status | `crates/harness-workflow/src/runtime/reducer/prompt_task_completion.rs:30,166` |
| Existing per-activity reducer check to generalize | `reducer/pr_feedback_completion.rs:62` (`pr_feedback_success_contract_error`) |
| Terminal-evidence precedent | `crates/harness-workflow/src/runtime/validator_github_issue_pr.rs:58-97` |
| Empty-window 100/A grading | `crates/harness-observe/src/quality.rs:26` |

## Component Changes

### 1. Evidence population (`harness-workflow`)

Enforcement already exists and is general:
`DecisionValidator::validate_decision` calls
`validator_progress::validate_declarative_transition_metadata` for every
matched rule, and that function rejects any decision missing a kind from the
rule's `required_evidence` set with `MissingRequiredEvidence`, for built-in
and declarative definitions alike (exempting same-state
`retry_failed_runtime_activity` decisions). No enforcement-path change is
required or made.

The work is therefore population plus producers:

- `runtime/validator.rs`: add a builder method
  `require_evidence(from, to, [classes])` and populate the Evidence Contract
  table from `product.md` in `github_issue_pr_defaults`,
  `prompt_task_defaults`, `quality_gate_defaults`, `pr_feedback_defaults`.
  Population alone activates enforcement on the next decision through the
  existing path.
- `runtime/store/runtime_completion.rs`
  (`declarative_decision_missing_required_evidence`) stays as-is: it is a
  declarative-only pre-check layered above the general validator and its
  behavior must remain byte-compatible.
- Evidence classes are matched by `WorkflowEvidence` kind string
  (conjunctive `BTreeSet` — see Component 4 for how the one disjunctive rule
  is encoded). New kinds: `verified_pr_binding`, `server_validation_digest`,
  `prompt_completion_evidence`. Kinds are constants in `runtime/model.rs`.
- The retry exemption is intentional and preserved: evidence requirements
  bind fact-minting transitions, not same-state activity retries.

### 2. Server-side quality-gate validation (`harness-server`)

- New module `workflow_runtime_worker/server_validation.rs`:
  `run_validation_commands(workspace_root, commands, timeout) -> ValidationDigest`
  where `ValidationDigest { command, cwd, exit_code, output_sha256,
  duration_ms, truncated }` per command, executed sequentially via the
  existing process-spawn utilities (no shell interpolation: commands run via
  the same argv-splitting path used elsewhere; commands come from
  `WORKFLOW.md`, which is repo-owned configuration).
- Integration point: when the worker completes a `run_quality_gate` activity,
  it executes the commands itself in the leased workspace *after* the agent
  turn, attaches the digest as `server_validation_digest` evidence on the
  `ActivityResult`, and the `quality_gate` reducer requires that evidence for
  `QualityPassed` to become `checking → passed`.
- Failure modes: non-zero exit → `QualityFailed` with digest; spawn error or
  timeout → blocked with a typed startup-error evidence record; no commands
  configured → blocked with `validation_commands_missing` (never a pass).
- Workspace note: reuses the activity's existing workspace lease; the run
  happens before lease release in the same worker tick.

### 3. Verified PR binding (`harness-server` + `harness-workflow`)

- The reducer cannot call GitHub (workflow crate stays IO-free). Split the
  flow: `bind_pr_from_activity_result` continues to produce the `BindPr`
  command, but command application in the server-side dispatch path performs
  verification via the existing GraphQL snapshot client
  (`github_pr_snapshot.rs`) before commit:
  - PR exists, state `OPEN`, repository matches the workflow subject.
  - Head ref matches the workspace branch recorded in workflow data (or, for
    deferred submissions, the candidate branch).
  - On success: attach `verified_pr_binding { pr_number, head_oid,
    observed_at }` evidence and apply the command.
  - On failure: emit a blocked decision with reason
    `pr_binding_verification_failed`; do not transition to `pr_open`; the
    intake coverage gate must not count the issue as covered.
- GitHub outage handling: verification errors (rate limit, 5xx) defer the
  command via the existing dispatch-defer mechanism rather than blocking the
  workflow, bounded by the standard retry policy.

### 4. Prompt-task completion evidence (`harness-workflow`)

The product rule is disjunctive (`validation_report` OR
`no_change_rationale`), but `TransitionRule::required_evidence` is a
conjunctive set — listing both kinds would reject every valid decision, and
listing neither enforces nothing. Chosen encoding: the reducer mints a
single umbrella evidence kind, and the transition requires only that kind.
This keeps the shared validator simple and conjunctive, avoids extending
`TransitionRule` (and the declarative YAML schema that feeds it) with
alternative-set semantics, and places the OR where the branching context
already lives.

- `reducer/prompt_task_completion.rs`: before returning
  `single_shot_done_decision` or `settled_done_decision`, check the result
  for a `validation_report` artifact (structured: list of `{command,
  exit_code}`) or a `no_change_rationale` string artifact. When either is
  present, attach `WorkflowEvidence::new("prompt_completion_evidence",
  <which alternative satisfied it>)` to the decision. Absent both, return
  the existing `blocked_decision` helper with reason
  `prompt_completion_evidence_missing`.
- `prompt_task_defaults` requires `prompt_completion_evidence` on
  `implementing → done`, so a done-decision that bypasses the reducer check
  still fails validation; the reducer check exists to give the agent a
  precise, retryable reason instead of a bare transition rejection.

### 5. Honest empty-window grading (`harness-observe`)

- `quality.rs::grade`: return a new `Grade::Unknown` variant (or
  `Option<QualityReport>`) when `events.is_empty() && violation_count == 0`.
  Sole consumer `quality_trigger.rs` treats `Unknown` as "no change to GC
  cadence".

## Data / Schema Impact

None. Evidence rides the existing `WorkflowEvidence` records on decisions;
digests are evidence payloads, not new tables. No migration.

## Configuration

- `runtime_worker.completion_evidence_enforced: bool` (default `true`) in
  `WORKFLOW.md` — one release of kill-switch, then removed. Applied to the
  built-in definitions at server startup, before the definition registry is
  frozen; a config that fails to load keeps enforcement on.
- Validation timeout: reuse the existing activity timeout configuration with
  a dedicated `quality_gate_validation_timeout_secs` override (default 900).

## Test Plan

- `harness-workflow` unit: transition table tests per definition (accept
  with evidence / reject without, reason codes asserted); prompt-task
  reducer tests for all three terminal paths; declarative regression via the
  unified check.
- `harness-server` unit: `server_validation.rs` digest correctness, timeout,
  spawn-failure, no-commands cases; `BindPr` verification against a mocked
  snapshot client (nonexistent / closed / wrong-repo / wrong-head / valid).
- Integration (Postgres-gated, existing harness): end-to-end quality-gate
  flow where agent claims pass and server run fails; end-to-end prompt task
  blocked on missing evidence; rejected decision leaves the instance row
  unchanged (snapshot comparison).

## Risks and Mitigations

- **Legitimate work blocked** (agents not yet emitting `validation_report`):
  reason codes are precise and retryable; the prompt packet already tells
  agents the contract (`activity_contract.rs:143`), and the kill switch
  covers the transition release.
- **GitHub API dependence at bind time**: deferred (not blocked) on
  transport errors; only definitive negative answers block.
- **Double execution of validation commands** (agent ran them, server
  re-runs): accepted cost — the server run is the authoritative one; agents
  may skip self-running gate commands once the contract documents the server
  re-run.
- **Workspace state divergence** (server validates a tree the agent mutated
  after its claimed run): inherent to re-verification and is the point — the
  digest reflects the tree that will be pushed.
