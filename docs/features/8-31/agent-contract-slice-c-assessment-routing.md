# Agent Contract Slice C: Assessment and Routing

Date: 2026-08-31

Status: Implemented; final PR review and CI pending

Scope: close the production workflow loop for the pinned agent-contract primitive delivered in Slice B

## Summary

Slice C replaces the unconditional dispatcher barrier with an exact, capability-aware authorization. A contract command can create a runtime job only when the server has selected a concrete runtime profile with a positive timeout and the backend instance that will execute it claims every contract-enforcement capability.

The runtime now reserves every primary or correction attempt durably before invoking the model, validates the observed result, writes one server-authored assessment for a valid verdict, and routes only the assessment outcome through the pinned definition's exact `on_signal` mapping. A model-authored signal cannot route a contract activity.

Slice D remains separate. This change does not claim a production server restart dogfood run or operational latency/cost evidence.

## Production path

```text
declarative submission
  -> pinned EnqueueActivity command
  -> project runtime-profile selection
  -> concrete backend capability authorization
  -> runtime job
  -> durable AgentContractAttemptStarted reservation
  -> isolated observed agent attempt
  -> optional bounded structured-output correction
  -> server-authored assessment artifact
  -> pinned assessment validation
  -> exact on_signal route
  -> transactional workflow event, decision, and evidence
```

The command remains the immutable execution snapshot. It carries the contract, prompt, semantic input, input provenance, contract hash, definition hash, and budgets produced by Slice A and Slice B. Before dispatch, the server derives the authorized activity from the persisted instance state and pinned declarative definition, reconstructs the expected command, and requires exact equality. Removing the contract marker or substituting another activity cannot downgrade execution to the ordinary path. Before any executor preflight, the runtime job snapshot is also required to match that authorized command exactly.

## Capability-aware dispatch

The workflow dispatcher remains fail-closed by default. The server authorizes only the exact selected profile name after verifying:

- the pinned contract parses and validates;
- the output schema has a canonical enforcement document;
- the effective runtime profile has a positive timeout;
- the runtime kind maps to a locally executable backend;
- the concrete backend instance claims prompt-only launch, pinned output schema, and attempt observation.

Effective-profile rewrites are applied before the dispatcher compares the authorization. An eval override cannot authorize one profile and enqueue a different runtime kind or profile.

A missing backend, incomplete capability claim, remote runtime, zero timeout, malformed contract, or profile mismatch leaves the command behind `agent_contract_enforcement_unavailable` and creates no runtime job. A contract that is malformed or differs from the pinned definition or instance fails the command and workflow fatally. Runtime-job construction hashes the selected profile snapshot; executor preflight validates that hash plus the job's activity, profile name, and runtime kind before exact-replay and disabled-worker checks, then repeats the backend check as defense in depth.

## Durable attempt budget

Every model invocation requires a transactionally persisted `AgentContractAttemptStarted` event keyed by primary and correction indexes. The reservation method reads the budget from the contract already pinned in the runtime job; callers do not supply a second budget.

If the exact reservation already exists after a crash or lease reclaim, execution fails fatally without invoking the model again. This conservative rule may spend a reserved attempt whose process never started, but it cannot silently exceed the pinned budget.

Each completed attempt also writes `AgentContractAttemptCompleted` with its indexes, status, raw output, and validation error. A structured-output failure may consume the pinned correction allowance. Tool or mutation evidence and execution errors are fatal and cannot fall into the generic declarative retry path.

## Server assessment

A valid verdict produces exactly one `agent_contract_assessment` artifact. The assessment records:

- deterministic assessment, command, and runtime-job identities;
- activity, definition, contract, and semantic-input hashes;
- runtime profile and kind;
- the validated outcome and raw verdict;
- pinned primary/correction limits and actual consumption.

The raw verdict remains a separate `agent_contract_verdict` artifact. Server observations remain separate per-attempt artifacts. This keeps model-authored claims distinct from server-authored validation and execution facts.

## Routing and replay

The declarative reducer does not use ordinary `ActivitySignal` precedence for a contract state. It revalidates the single assessment against the persisted completion event and pinned definition:

- command type and activity;
- exact pinned contract, prompt, and definition hash;
- canonical semantic input and its hash;
- assessment, command, job, profile, and runtime identities;
- canonical verdict schema and exact allowed outcome vocabulary;
- raw-verdict equality;
- pinned and consumed budgets.

Only the validated outcome selects the corresponding `on_signal` target. Missing, duplicate, malformed, forged, unknown, or budget-inconsistent assessments fail closed through the existing invalid-agent-output path.

Completion still uses the existing atomic runtime-job, workflow-event, decision, and evidence transaction. Reopening the store reconstructs the same state and decision from persisted data without another model call.

## Focused verification

Fresh evidence from this branch:

| Surface | Result |
|---|---|
| Canonical contract tests | 9 passed |
| Declarative contract tests | 19 passed |
| Server `contract` filter | 90 passed, 1 ignored live dogfood |
| Runtime dispatch tests | 28 passed |
| Exact-replay preflight test | 1 passed |
| Real submission, assessment, route, and store reopen | passed; one model-backend invocation |
| Durable correction | invalid primary plus one valid correction passed; two persisted reservations |
| Reclaimed reservation | failed without a duplicate model invocation |
| Lock-order tests | 23 passed, 4 isolated stress tests ignored by the focused command |
| Formatting and patch whitespace | passed |
| `harness-workflow` and `harness-server` all-target checks | passed |
| Workspace clippy with warnings denied | passed |
| Real Codex Slice B dogfood | `gpt-5.6-sol`, 1 passed in 9.96 seconds |

Repository pre-push, independent fresh-context review, and GitHub `CI Result` remain merge gates and are not claimed by this record yet.

The first independent review found three blockers. The remediation binds authorization to the complete post-rewrite runtime profile, classifies every infrastructure error from the dedicated contract path as fatal, and snapshots the persisted runtime job identity plus exact attempt reservations into the atomic completion event for replay validation. A new independent review is still required.

The second independent review found that server-owned activity dispatch still preceded contract extraction and that malformed pinned payloads could escape the typed fatal boundary. Contract extraction now runs first for every activity name, including server-owned name collisions, and every present-contract extraction error is wrapped as a fatal contract execution error. A further fresh-context review remains required.

The third independent review found three remaining upstream gaps. Malformed present contracts now fail their claimed command instead of entering a permanent dispatch barrier; contract presence takes precedence over server-owned activity names for runtime-turn budgeting and disabled-worker policy; and fatal or configuration contract failures terminate instead of following `on_failure` into a new contract budget. A further fresh-context review remains required.

The fourth independent review found five remaining lifecycle gaps. Contract validation now precedes project policy and config resolution; invalid dispatch atomically records command failure, completion, and the reducer decision; every model invocation atomically consumes one workflow turn and one contract attempt; reservations require the current running lease owner and generation; and the contract stream terminates on lease loss. A further fresh-context review remains required.

The fifth independent review found two validation-order gaps. Dispatcher and executor now share the same complete pinned-envelope validation, including prompt, definition hash, semantic input, canonical output schema, and contract hash. That validation finishes before project policy resolution and before any workflow-turn or attempt reservation. A further fresh-context review remains required.

The sixth independent review found two remaining authorization-order gaps. Dispatch now binds the complete contract command to the persisted workflow instance and its hydrated pinned declarative definition, including the exact contract, prompt, definition hash, subject, facts, and provenance. Executor preflight then binds the runtime job snapshot back to that authorized command before exact-replay or disabled-worker handling. A final fresh-context review remains required.

The seventh independent review found three downgrade and lifecycle gaps. Contract expectation now comes from the pinned definition and current instance state rather than mutable command/job markers. The job snapshot binds activity, command identity, profile hash, profile name, and runtime kind before any other preflight. Finally, closing the backend event channel no longer escapes timeout or lease-loss selection: the runtime keeps the backend future fenced until completion or cancellation. A further fresh-context review remains required.

The eighth independent review found four final dispatch and lifecycle gaps. Timeout and lease-loss selection is now fair under a continuously ready event stream while completed streams still drain every buffered observation. Pinned-command validation runs before the mutable command type can select the non-runtime skip path. Fatal contract corruption terminates from the pinned state contract even when persisted completion identities are malformed. Backend resolution is runtime-kind aware, so `CodexJsonrpc` authorizes and executes a fresh per-turn JSON-RPC backend instead of the Codex oneshot singleton. A further fresh-context review remains required.

The ninth independent review found a combined-marker downgrade under internally consistent persistence corruption. Normal classified writes, submission construction, and command enqueue already reject removal of a declarative definition pin. The dispatcher now also treats the immutable persisted definition kind as authoritative when both mutable contract markers are missing, so a coherently corrupted instance and command cannot select either ordinary execution or the non-runtime skip path. A further fresh-context review remains required.

## Slice D boundary

Slice D must start from the merged Slice C commit and use the production server dispatch path with a real Codex-only submission. It must capture restart/replay behavior plus latency, token, and cost evidence. It must not broaden this contract into automated merge authorization or the unrelated vNext phases.
