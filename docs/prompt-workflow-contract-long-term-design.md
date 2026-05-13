# Prompt Workflow Contract Long-Term Design

Status: Draft
Date: 2026-05-13

Related documents:

- `docs/workflow-runtime-decoupling-plan.md`
- `docs/workflow-runtime-hardening-design.md`
- `docs/local-codex-review-provider-spec.md`
- `docs/codex-app-server-reference.md`

## Goal

Make Harness reliable for long-running autonomous work by turning prompts from best-effort
instructions into versioned workflow contracts.

The long-term design keeps the current workflow runtime direction: the server owns state,
agents propose facts and decisions, and reducers validate every state transition. The missing
piece is a durable contract boundary around prompts and agent output:

```text
Project workflow definition
  -> PromptSpec compiler
  -> RuntimePromptPacket
  -> AgentAdapter
  -> ActivityResultEnvelope
  -> Output repair and validation
  -> Reducer decision
  -> Workflow event and command outbox
```

The system should tolerate normal agent variability without silently burning quota, losing parent
workflow progress, or requiring operators to discover contract drift only after an overnight run.

## Problem Statement

Harness currently has the right high-level architecture, but the prompt boundary is still too soft.
A runtime agent can complete a turn successfully while failing the workflow contract. For example,
the agent may return ordinary prose, place JSON in the wrong block, omit required signals, emit a
valid-looking but semantically empty result, or produce a child workflow decision that is stripped
before the parent can consume it.

These are not ordinary model quality issues. They are contract failures between the agent runtime
and the workflow runtime. Contract failures must be classified, repaired when safe, suppressed when
repetition cannot help, and surfaced through health/status tooling.

## Design Principles

1. **Workflow state is server-owned.** Agents never mutate workflow state directly. They only return
   candidate facts, artifacts, validation results, and decisions.
2. **Prompt text is compiled, not hand-assembled.** Workflow definitions and activity contracts
   compile into prompt packets, schemas, examples, and reducer expectations from one source of
   truth.
3. **Structured output is mandatory but adapter-specific.** Use native structured output or tool
   result channels where available. Keep fenced JSON as compatibility, not the only contract.
4. **Semantic empty success is invalid.** A successful activity must emit the signals or artifacts
   required by its activity contract, or an explicit no-op signal that proves no work exists.
5. **Every completed turn resolves.** A completed agent turn must become exactly one of:
   `accepted`, `repaired`, `blocked`, `retry_scheduled`, or `terminal_failed`.
6. **Repair is bounded and observable.** Repair can normalize formatting or classify an otherwise
   useful result, but it cannot invent domain facts.
7. **Parent and child workflows exchange typed outcomes.** Child completion must propagate a
   normalized outcome envelope. Parents should not depend on duplicated ad hoc signal strings.
8. **Operators need first-class health signals.** Status endpoints must show stalled workflows,
   invalid output, repair attempts, suppressions, retry cooldowns, and capacity pressure.
9. **Repo owners can change workflow policy safely.** Project-owned workflow files should be
   validated before dispatch and hot-reloaded without server restart.
10. **Backward compatibility is explicit.** Existing built-in workflow definitions continue to work
    while project workflow files and structured adapter output roll out incrementally.

## Lessons From Symphony

Symphony has a simpler workflow model, but several design choices are worth adopting:

| Area | Symphony approach | Harness long-term adoption |
|---|---|---|
| Workflow ownership | Repo-owned `WORKFLOW.md` contains config and prompt policy. | Add project workflow definitions that compile into typed Harness contracts. |
| Template validation | Unknown template variables fail before dispatch. | Validate workflow prompt templates during project load and reload. |
| State split | Orchestrator state is separate from tracker state. | Keep workflow runtime state separate from GitHub/issue tracker state. |
| Multi-turn loop | After each turn, re-check external state and continue until done or max turns. | Keep runtime turn budgets and make continuation decisions explicit workflow commands. |
| Retry/backoff | Turn failures use bounded backoff. | Keep retry policy, but add contract-failure suppression and repair counters. |
| Tool protocol | Dynamic tools have schemas and normalized result envelopes. | Normalize all provider/adapter outputs through `ActivityResultEnvelope`. |

Symphony does not fully solve machine-verified final business output. Its final response policy is
still mostly prompt-driven. Harness should take the stronger path: keep the typed workflow runtime,
but make prompt and output contracts hard enough for unattended operation.

## Target Architecture

```text
Source adapter
  -> WorkflowEvent
  -> WorkflowController
  -> WorkflowDefinitionRegistry
  -> ActivityContract
  -> PromptSpecCompiler
  -> RuntimePromptPacket
  -> RuntimeJob
  -> AgentAdapter
  -> RawAgentTurn
  -> ActivityResultExtractor
  -> ActivityResultRepair
  -> ActivityResultValidator
  -> Reducer
  -> WorkflowDecision
  -> WorkflowCommand outbox
```

### WorkflowDefinitionRegistry

The registry owns workflow definitions from two sources:

- built-in definitions compiled into Harness
- project workflow definition files loaded from configured project paths

Recommended project file lookup order:

1. explicit `projects.<name>.workflow_file`
2. `.harness/WORKFLOW.md`
3. `WORKFLOW.md`
4. built-in workflow defaults

The registry returns a validated `WorkflowDefinitionBundle`:

```text
WorkflowDefinitionBundle
  workflow_id
  version
  states
  terminal_states
  activities
  transition_rules
  command_rules
  retry_policy
  prompt_templates
  status_mapping
  provider_policy
```

Invalid project definitions are configuration failures and must block dispatch. They should not
silently fall back to defaults unless the project explicitly enables fallback.

### ActivityContract

Each runtime activity must have a typed contract:

```text
ActivityContract
  workflow_id
  activity
  expected_current_states
  allowed_next_states
  required_signals
  optional_signals
  required_artifacts
  optional_artifacts
  explicit_noop_signals
  validation_commands
  child_outcome_contract
  retry_policy
  repair_policy
  examples
```

This contract is the single source for:

- prompt packet instructions
- JSON schema generation
- reducer semantic guards
- test fixtures
- status/diagnostic rendering

The existing drift where prompt text describes fields that do not match `ActivityResult` must be
eliminated. Prompt-visible schema must be generated from the same Rust contract used by validation.

### PromptSpecCompiler

The compiler converts a workflow definition and runtime job context into a `RuntimePromptPacket`.

Inputs:

- workflow instance snapshot
- activity contract
- runtime profile
- project workflow template
- source event context
- available commands and transition rules
- previous attempts and repair history

Outputs:

- prompt packet JSON
- human-readable prompt body
- strict output schema
- activity-specific examples
- digest and provenance metadata

The packet should be versioned and recorded before execution. Any prompt packet change should be
traceable through its digest and compiler version.

### RuntimePromptPacket

The packet should include:

```text
schema = "harness.runtime.prompt_packet.v2"
workflow_id
workflow_instance_id
activity
contract_version
current_state
allowed_transitions
required_commands
required_signals
required_artifacts
explicit_noop_signals
output_schema
repair_policy
project_instructions
activity_prompt
examples
```

Prompt text should be short and repetitive by design. The agent should see the same output contract
near the start and again at the end. The final section should be mechanically generated from the
contract, not manually written per activity.

### AgentAdapter

Adapters should expose capability metadata:

```text
AgentAdapterCapabilities
  supports_structured_output
  supports_tool_result_output
  supports_jsonrpc_turn_events
  supports_continuation
  supports_review_mode
  supports_reasoning_effort
  supports_timeout
  supports_sandbox_policy
```

Output strategy order:

1. Native structured output or tool result channel.
2. App-server JSON-RPC event result when available.
3. Final fenced `harness-activity-result` JSON block.
4. Legacy compatibility parser that can identify a likely JSON object and pass it to repair.

The runtime should record which strategy produced the result. Fenced JSON should remain supported,
but it should not be the only successful path.

### ActivityResultEnvelope

Raw agent output should first become an envelope:

```text
ActivityResultEnvelope
  extraction_strategy
  raw_turn_id
  raw_status
  extracted_result
  extraction_errors
  repair_attempts
  validation_errors
  final_result
```

This separates three facts that are currently easy to confuse:

- the agent process completed
- Harness extracted a candidate result
- the candidate result passed the activity contract

The reducer should only receive `final_result` after validation, or a typed invalid-output result
when validation cannot succeed.

### ActivityResultRepair

Repair is allowed only when it preserves facts already present in the raw turn. It may:

- move JSON from a generic block into the expected envelope
- normalize field names from a known compatibility alias
- convert clear native review text into a normalized review report
- classify a missing final block as `InvalidAgentOutput`
- extract validation command results already present in the transcript

Repair must not:

- invent issue numbers, PR numbers, mergeability, approvals, or CI state
- convert ambiguous prose into success
- fabricate validation commands that were not run
- create a parent transition when child outcome evidence is missing

Repair outcomes:

| Repair outcome | Meaning |
|---|---|
| `repaired` | A valid result was recovered from raw output. |
| `blocked_invalid_output` | The turn completed, but no safe result can be produced. |
| `retry_invalid_output` | The output failed due to a retryable adapter or provider problem. |
| `terminal_invalid_output` | The same prompt/adapter contract repeatedly failed and is suppressed. |

### Reducer Invariants

Reducers must enforce these invariants:

1. A valid structured `workflow_decision` wins only after transition and command validation.
2. Activity-specific reducers may derive decisions from typed signals and artifacts.
3. Invalid structured decisions become blocked decisions with operator attention.
4. Empty success is blocked unless an explicit no-op signal is valid for that activity.
5. Child workflow completion propagates a typed child outcome envelope to the parent.
6. Parent workflows consume child outcomes directly, not stripped artifacts plus duplicated signals.
7. Terminal states are never ordinary scheduler candidates.
8. Repeated invalid-output failures trigger cooldown or suppression, not unbounded polling.

## Project Workflow File

The project workflow file should be Markdown with YAML front matter:

```markdown
---
schema: harness.project_workflow.v1
workflow_id: github_issue_pr
max_turns: 12
max_concurrent_workflows: 4
retry:
  max_attempts: 3
  max_backoff_secs: 1800
providers:
  implementor: codex_exec
  reviewer: codex_cli_review
status_mapping:
  backlog: ["Backlog", "Todo"]
  active: ["In Progress", "Rework"]
  review: ["Human Review"]
  done: ["Done"]
---

## Activity: implement_issue

Project-specific implementation policy.

## Activity: inspect_pr_feedback

Project-specific review feedback policy.
```

The front matter controls validated runtime settings. The Markdown body controls project policy and
activity-specific instructions. The compiler should reject unknown required keys and unknown
template variables.

## Workflow-Specific Contracts

### `repo_backlog`

Required outcomes:

- `IssueDiscovered`
- `IssueSkipped`
- `NoOpenIssueFound`

`plan_repo_sprint` must produce one of:

- `SprintTaskSelected`
- `IssueSkipped`
- `NoSprintTaskSelected`
- valid `sprint_plan` artifact with selected tasks or explicit no-op evidence

Invalid empty success must block and suppress repeated sweeps for a bounded cooldown.

### `github_issue_pr`

Required outcomes depend on state:

- implementation states must bind a PR artifact, produce a blocked decision, or produce validation
  evidence for no-code/no-PR completion when the workflow allows it
- PR feedback states must consume `pr_feedback` child outcomes
- ready-to-merge states must require local review and CI evidence according to review policy

The workflow should not rely on direct GitHub/git calls inside Rust. GitHub and git inspection
remain agent/runtime responsibilities expressed through prompts and normalized results.

### `pr_feedback`

The child workflow should return a typed `PrFeedbackOutcome`:

```text
PrFeedbackOutcome
  status = feedback_found | no_feedback_found | changes_requested | checks_failed | ready_to_merge | blocked
  comments
  check_runs
  review_state
  mergeability
  evidence
```

The parent should consume this outcome directly. The child should not lose valid decision artifacts
before parent propagation.

### `prompt_task`

Prompt-only tasks should return:

- implementation summary
- changed files when applicable
- validation results
- blockers or explicit no-op evidence

The task should not be marked done solely because the agent process completed.

### `quality_gate`

Quality gate activities should return:

- gate name
- commands run
- pass/fail state
- findings
- blocking severity
- suggested next activity when blocked

## Status And Health Model

Harness needs a first-class status surface for operators and for self-healing logic.

Minimum status categories:

| Category | Examples |
|---|---|
| Runtime health | running jobs, queued jobs, stalled streams, timeout counts |
| Contract health | missing result blocks, invalid schemas, repair successes, repair failures |
| Workflow health | blocked workflows, terminal failures, retry cooldowns, child workflows awaiting parent propagation |
| Capacity health | domain limits, project limits, pool wait, queue age |
| Provider health | Codex CLI availability, app-server availability, review provider status |
| Suppression health | suppressed sweeps, suppression reason, next allowed attempt |

Recommended endpoints:

- `GET /health`: lightweight liveness and dependency state
- `GET /status`: operator summary across projects and domains
- `GET /api/workflow-runtime/diagnostics`: detailed contract and reducer diagnostics
- `GET /api/workflow-runtime/instances/:id/trace`: prompt packet, extraction envelope, reducer decision, commands

## Testing Strategy

Every activity contract should generate or reference fixtures.

Required test groups:

| Test group | Expectation |
|---|---|
| Prompt compilation | Unknown template variables fail before dispatch. |
| Schema drift | Generated prompt schema matches `ActivityResult` and activity contracts. |
| Extraction | Native structured output, fenced JSON, generic JSON, and missing output are classified correctly. |
| Repair | Safe formatting repair succeeds; ambiguous prose blocks. |
| Reducer invariants | Empty success, invalid decision, missing commands, and terminal candidates are rejected. |
| Parent/child propagation | Child outcomes advance or block parents deterministically. |
| Suppression | Repeated invalid-output failures do not enqueue unbounded sweeps. |
| Status API | Diagnostics expose stuck workflows and contract failure counts. |
| End-to-end simulation | Repo backlog, issue PR, PR feedback, prompt task, and quality gate run through synthetic turns. |

## Migration Plan

### Phase 1: Contract Inventory

- Define `ActivityContract` for every existing runtime activity.
- Generate prompt-visible schemas from runtime types.
- Remove manually duplicated schema fragments that can drift.
- Add fixture tests for all current activity output examples.

### Phase 2: Extraction Envelope

- Introduce `ActivityResultEnvelope`.
- Record extraction strategy and validation errors.
- Keep existing fenced JSON parser as one extraction strategy.
- Add compatibility parsing for native structured output and generic JSON blocks.

### Phase 3: Repair And Suppression

- Add bounded repair for formatting-only failures.
- Add invalid-output cooldowns by workflow/activity/project.
- Surface repair and suppression metrics in runtime events.

### Phase 4: Parent/Child Outcome Model

- Add typed child outcome artifacts for `pr_feedback`.
- Stop relying on stripped decisions plus duplicated signals.
- Make parent consumption deterministic and tested.

### Phase 5: Project Workflow Definitions

- Add project workflow file loader and validator.
- Support `.harness/WORKFLOW.md` and explicit configured workflow files.
- Compile project policy into prompt packets.
- Hot-reload changed workflow definitions after validation.

### Phase 6: Status And Diagnostics

- Add `/status` and workflow runtime diagnostics.
- Include contract failures, queue pressure, suppression state, and stuck workflow detection.
- Add dashboard rendering after the API shape is stable.

### Phase 7: Adapter Capability Upgrade

- Add adapter capability probes.
- Prefer native structured output or tool result channels.
- Keep fenced JSON compatibility until all supported adapters have better output channels.

## Acceptance Criteria

The long-term design is complete when:

1. No runtime activity relies on manually duplicated prompt schema.
2. Every runtime activity has an `ActivityContract`.
3. Every completed agent turn resolves to accepted, repaired, blocked, retry scheduled, or terminal
   failed.
4. Missing or malformed output cannot cause unbounded polling or quota burn.
5. Parent workflows consume typed child outcomes.
6. Project workflow definitions can be validated before dispatch and reloaded safely.
7. Operators can answer "what is stuck and why" from `/status` without inspecting database tables.
8. End-to-end tests cover repo backlog, issue PR, PR feedback, prompt task, and quality gate flows.

## Non-Goals

- Do not make agents deterministic.
- Do not move product judgment into Rust reducers.
- Do not call `git` or `gh` directly from Harness crates.
- Do not require every project to adopt project workflow files on day one.
- Do not remove existing built-in workflow definitions during the migration.
