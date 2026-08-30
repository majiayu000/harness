# Agent Contract Slice B: Implementation and Validation Record

Date: 2026-08-30

Status: Implemented; draft PR pending CI and independent review

Scope: Slice B — enforce one pinned agent-contract attempt through the `AgentBackend` surface

## 1. Executive summary

Slice B turns the contract declaration introduced by Slice A into a real, bounded agent attempt. A declarative workflow now produces an immutable semantic input envelope, the server validates that envelope against the canonical input schema, launches a conforming Codex backend in a fresh empty workspace, observes the complete attempt stream, validates the structured verdict against the canonical output schema and the pinned outcome vocabulary, and returns server-authored evidence about the attempt.

This slice deliberately does not enable automatic workflow dispatch. The dispatcher still places every contract-bearing command behind `agent_contract_enforcement_unavailable`. Slice C must own server-issued assessment, outcome routing, correction/retry accounting, replay, and the capability-aware replacement of that temporary barrier.

The implementation has one important Codex-specific qualification: `codex exec` has no supported flag that physically removes its tool surface. Harness therefore combines a read-only sandbox, `approval_policy = never`, an empty declared tool allowlist, a fresh empty workspace, prompt isolation, and full stream observation. Any observed tool activity invalidates the attempt. Provider network egress remains enabled so the Codex CLI can reach the model service; this host permission is not authorization for model-facing tools.

## 2. Slice boundaries

The planned delivery sequence is:

| Slice | Responsibility | State after this change |
|---|---|---|
| A | Declare, validate, persist, hash, and pin an agent contract in workflow commands | Merged in PR #2020 |
| B | Execute and observe one pinned attempt through `AgentBackend` | Implemented in this branch |
| C | Issue server-authored assessments, route verdicts, enforce correction budgets, and replay deterministically | Not implemented |
| D | Run a Codex-only end-to-end dogfood workflow through production dispatch | Not implemented |

Slice B proves the execution primitive. It does not prove the complete workflow loop.

## 3. Why the implementation was corrected

An earlier version had three evidence and architecture problems:

1. A fake `codex` executable returned prepared JSON. That only tested parser wiring, not an installed Codex runtime or model behavior.
2. Tests manually inserted `agent_contract_input`, while production workflow command construction did not produce it. That created a declaration-to-execution gap.
3. The runtime repeated input and verdict field checks in handwritten Rust even though the canonical JSON Schema documents already defined the boundary.

The fake bridge test was removed. The production workflow is now the input producer, both validation directions use the canonical schema documents, and an explicitly ignored live dogfood test invokes the installed and authenticated Codex CLI with `gpt-5.6-sol`.

Two further blockers were found and closed during correction:

- Canonical `minLength: 1` accepted whitespace-only strings. The schemas now require at least one non-whitespace character for semantic identifiers, hashes, rationales, and evidence references.
- The dedicated attempt waited indefinitely for the event channel to close. It now requires a positive pinned runtime timeout, applies that timeout to the request, enforces an independent wall-clock deadline, aborts the stream task on expiry, and awaits cancellation.

## 4. End-to-end data path

The implemented path is:

```text
declarative workflow definition + pinned workflow instance
        |
        | build immutable EnqueueActivity command
        v
contract + prompt + definition_hash + semantic input envelope
        |
        | command outbox / runtime job payload
        v
dispatcher barrier (still closed until Slice C)
        |
        | dedicated execution primitive, exercised directly by tests
        v
agent-contract preflight
        |
        | canonical input validation + contract-hash verification
        v
isolated AgentRequest to a capability-conforming AgentBackend
        |
        | fresh empty workspace, read-only, approvals never,
        | no user config/rules, pinned schema, bounded timeout
        v
complete AgentEvent observation stream
        |
        | tool-use detection + canonical verdict validation
        v
ActivityResult + server-authored observation evidence
```

The command payload is the transactionally pinned boundary. Attempt execution does not consult a live workflow definition, repository checkout, prompt packet, memory document, user request, or mutable external fact source.

## 5. Pinned semantic input

### 5.1 Producer

`crates/harness-workflow/src/runtime/declarative_agent_contract.rs` builds the contract-bearing `EnqueueActivity` command. For contract activities it now requires the pinned `WorkflowInstance` and emits:

```json
{
  "activity": "classify",
  "agent_contract": { "...": "pinned contract" },
  "prompt": "pinned activity prompt",
  "definition_hash": "pinned workflow definition identity",
  "agent_contract_input": {
    "schema": "harness.semantic_activity_input.v1",
    "subject": {
      "kind": "pinned subject type",
      "identity": "pinned subject key"
    },
    "facts": { "...": "workflow instance data" },
    "provenance": { "...": "full pinned data provenance" },
    "contract_hash": "stable hash of the typed pinned contract"
  }
}
```

The same shared command constructor is used for:

- initial declarative submission;
- state-to-state activity transitions;
- operator recovery and unblock planning.

If instance data provenance is absent, contract command construction fails clearly. It does not invent provenance or silently omit the envelope.

### 5.2 Consumer

`PinnedJobAgentContract` requires `agent_contract_input`. Preflight rejects missing input, an unsupported schema, an invalid schema instance, or a contract hash that does not equal the stable hash of the pinned typed contract.

The only model-visible prompt is:

1. the pinned activity instruction; followed by
2. the immutable semantic input envelope serialized as JSON.

No workflow document, repository state, prompt packet, memory, user configuration, or rule file is appended.

## 6. Canonical schema boundary

The canonical input and output documents live in:

`crates/harness-core/src/config/workflow/agent_contract_schemas.rs`

The same documents are used for both purposes:

- runtime validation of untrusted input/output values;
- the structured-output schema handed to a supporting agent backend.

The implementation uses `jsonschema` 0.52 with default features disabled. This avoids maintaining a second handwritten interpretation of required fields, field types, unknown-field policy, string constraints, and array item constraints.

### 6.1 Input invariants

The v1 input schema requires:

- the exact input schema identifier;
- a subject with non-whitespace `kind` and `identity`;
- an object-valued facts snapshot;
- an object-valued provenance snapshot;
- a non-whitespace typed-contract hash;
- no unknown top-level or subject fields.

### 6.2 Verdict invariants

The v1 verdict schema requires:

- the exact verdict schema identifier;
- a non-empty, whitespace-free outcome token;
- a rationale containing at least one non-whitespace character;
- an `evidence_refs` array, which may be empty;
- non-whitespace evidence references when present;
- no unknown fields.

After schema validation, the server separately verifies that `outcome` belongs to the exact vocabulary pinned in `allowed_outcomes`. The schema describes the stable envelope; the pinned contract supplies the activity-specific vocabulary.

### 6.3 OpenAI structured-output compatibility

Live Codex dogfood exposed two stricter structured-output requirements:

- properties constrained by `const` still need an explicit JSON type;
- every declared property must be listed as required.

The canonical documents now satisfy those requirements. `evidence_refs` is therefore required and uses an empty array when the model has no evidence references.

## 7. Backend capability and launch contract

Before launch, the server requires the backend to advertise all three capabilities:

- prompt-only launch;
- pinned output schema support;
- complete attempt observation stream.

Codex cloud execution cannot claim prompt-only launch because cloud setup and container state are applied before the attempt. Cloud-enabled `CodexAgent` instances therefore fail capability preflight.

For a conforming local `CodexAgent`, the request pins:

- model and reasoning effort from the selected runtime profile;
- a positive timeout from that profile;
- `allowed_tools = []`;
- read-only sandbox mode;
- approval policy `never`;
- a fresh empty temporary workspace;
- a schema file outside the workspace;
- ignored user config and rule files through the Codex launch contract;
- ephemeral execution;
- the pinned prompt and input envelope only.

`AgentPermissionMode::Full` is intentionally used at the host egress decision point because the Codex process needs provider network access. Using `Scoped` with an empty allowlist mapped host egress to deny-all and caused the CLI to wait until timeout. This value does not relax the model-facing sandbox or tool policy listed above.

## 8. Observation and fail-closed behavior

The dedicated attempt records the complete `AgentEvent` stream. Direct `AgentEvent::ToolCall` values and tool-like completed items are evidence of prohibited activity. An observed violation invalidates the attempt even if a syntactically valid verdict follows.

`CodexAgent::execute_stream` previously ended with `Done` without emitting the parsed final reply as `TurnCompleted`. That was a production bug: downstream code could observe a successful process but receive no final output. It now emits `TurnCompleted { output }` before `Done`.

The attempt fails instead of degrading when any of these conditions occur:

| Condition | Result |
|---|---|
| Missing or invalid pinned input | Preflight failure |
| Input contract hash mismatch | Preflight failure |
| Backend lacks a required capability | Preflight failure |
| Runtime profile has no positive timeout | Preflight failure |
| Stream exceeds the wall-clock deadline | Stream is aborted and awaited; attempt fails |
| Backend stream task fails | Attempt fails |
| Any prohibited tool activity is observed | Attempt is invalidated |
| Reply is absent or is not JSON | Verdict failure |
| Reply violates the canonical output schema | Verdict failure |
| Outcome is outside the pinned vocabulary | Verdict failure |
| Cloud Codex is selected | Capability preflight failure |

Server-authored observation evidence is attached to the activity result so later slices can distinguish model claims from runtime observations.

## 9. Real Codex dogfood

The ignored dogfood test is:

`crates/harness-server/src/workflow_runtime_worker/agent_contract_dogfood_tests.rs`

It requires an installed, authenticated Codex CLI and deliberately does not run in the default unit-test suite. It launches:

- exact model: `gpt-5.6-sol`;
- reasoning effort: high;
- a fresh empty workspace;
- the canonical structured-output schema;
- the real contract-attempt implementation;
- a 300-second deadline.

The model is asked to classify a pinned semantic envelope using a small fixed outcome vocabulary. The passing run returned `small`, produced a schema-valid verdict, and generated no observed contract violation.

This proves that the current local Codex CLI, authentication path, structured-output interface, event parser, and contract-attempt implementation interoperate. It does not prove universal model compliance, cryptographic model attestation, or the Slice C workflow loop. The observed model identity is derived from the pinned launch request, not attested independently by the provider.

### 9.1 Dogfood debugging record

The first real run timed out. The investigation proceeded by testing competing hypotheses rather than extending the timeout:

1. Codex wrapper/session environment variables were removed. The timeout remained, so that hypothesis was rejected.
2. The equivalent direct Codex CLI command in an empty read-only directory completed in about ten seconds. The CLI, authentication, model, and schema path were therefore viable.
3. Request construction was traced. `AgentPermissionMode::Scoped` plus an empty allowlist resolved host provider egress to deny-all.
4. The request was corrected to preserve provider egress while keeping the model-facing read-only, never-approve, empty-tool, fully observed contract.
5. The next real response exposed the strict structured-output schema requirements described above. The canonical schema was corrected rather than weakening verdict validation.

The final explicit dogfood run passed in 12.23 seconds.

## 10. Verification evidence

All evidence below was produced fresh during this implementation session.

| Surface | Command or method | Result |
|---|---|---|
| Formatting | `cargo fmt --all` | Pass |
| Formatting check | `cargo fmt --all -- --check` | Pass |
| Patch whitespace | `git diff --check` | Pass |
| Workspace lint | `cargo clippy --workspace --all-targets -- -D warnings` | Pass; 9m05s |
| Canonical schema tests | Focused `harness-core` schema tests | 2 passed |
| Contract attempt tests | Focused `harness-server` contract-attempt filter | 17 passed, 1 ignored dogfood |
| Real Codex dogfood | Explicit ignored-test invocation | 1 passed; 12.23s |
| Declarative producer tests | Focused non-DB `harness-workflow` tests | 9 passed |
| Recovery persistence | Exact test against disposable PostgreSQL database | 1 passed |
| Dispatcher barrier | Exact test against disposable PostgreSQL database | 1 passed |
| Runtime worker integration | Agent-contract integration module against disposable PostgreSQL database | 3 passed |
| Dependency security | `cargo audit` | Exit 0; only two allowed pre-existing warnings |
| Required agent package suite | `cargo test --package harness-agents` | 360 passed, 7 ignored, 1 load-sensitive failure |

The disposable PostgreSQL database was created on the local Docker PostgreSQL service at port 55433 and dropped with force immediately after the three narrow suites. It contained test data only and is not recoverable.

### 10.1 Agent package test qualification

The full `harness-agents` package suite was run twice. Both runs passed 360 tests and ignored 7 tests, but the same app-server child-startup timing test exceeded its three-second threshold under extreme machine load. An exact rerun of that test passed. The machine load average was approximately 58 on 12 cores with unrelated compiler and JavaScript test processes active.

No assertion, timeout, test infrastructure, or production behavior was weakened to hide the failure. GitHub CI remains the authoritative clean-environment result.

### 10.2 Dependency assessment

Adding `jsonschema` changed the lock file by roughly 250 lines and introduced 24 transitive packages. Default features are disabled, and the selected graph does not introduce a new HTTP/TLS client stack. `cargo audit` reported no advisory attributable to the new dependency. Its two warnings are existing allowed dependency-chain findings:

- RUSTSEC-2026-0221 through `event-listener` / `sqlx`;
- yanked `spin` 0.9.8 through an existing dependency chain.

## 11. What Slice B does not solve

Slice B is not the full classifier feature and should not be presented as one. It does not yet provide:

- production dispatch of contract-bearing workflow commands;
- server-issued semantic assessment records;
- mapping of a validated verdict into declarative `on_signal` transitions;
- correction-attempt or retry-budget consumption;
- deterministic replay of assessment and routing decisions;
- operator/API presentation of the assessment lifecycle;
- automatic merge or authorization decisions;
- proof that every future `AgentBackend` conforms;
- provider-attested model identity;
- physical removal of the Codex CLI tool surface.

The dispatcher barrier is therefore intentional and remains the production safety boundary.

## 12. Slice C entry plan

Slice C may begin after the Slice B PR is opened. It should be developed as a stacked branch based on Slice B so review can proceed in parallel, but Slice C must not merge before Slice B and must absorb any Slice B review changes.

Slice C should remain limited to closing the workflow-runtime loop:

1. Replace the unconditional dispatcher barrier with a capability-aware gate. A command may create a runtime job only when the selected backend and runtime profile satisfy the pinned contract.
2. Convert a valid observed attempt into a server-authored assessment event. Preserve the raw verdict as evidence, but do not treat model-authored claims as server facts.
3. Route the assessment outcome only through the exact `on_signal` mapping compiled in Slice A. Unknown or missing outcomes fail closed.
4. Consume pinned attempt and correction budgets in workflow state, not in transient worker memory.
5. Persist enough evidence to replay without rerunning the model or consulting a mutable definition.
6. Make duplicate delivery idempotent through the existing command/event deduplication boundary.
7. Preserve operator recovery without rebuilding a command from defaults.
8. Add narrow PostgreSQL-backed tests for dispatch, successful assessment, invalid verdict, tool violation, timeout, duplicate delivery, recovery, and replay.

### 12.1 Slice C acceptance criteria

Slice C is complete only when all of the following are demonstrated:

- a real workflow submission produces the pinned input without test fabrication;
- a conforming selected runtime clears the dispatcher gate and a non-conforming runtime does not create a job;
- one valid verdict produces one immutable server assessment and one deterministic route;
- invalid output, prohibited tool activity, timeout, unknown outcome, and exhausted budget cannot enter a success route;
- replay reconstructs the same state and route without calling the model;
- recovery preserves the original contract, prompt, input, definition hash, and budgets;
- focused unit and isolated PostgreSQL integration tests pass;
- the Slice B real Codex dogfood remains green;
- fresh-context review and `CI Result` approve the stacked change before merge.

## 13. PR and merge readiness

The implementation is ready for a draft PR because its intended primitive is present, focused tests and real dogfood pass, and the remaining production barrier is explicit. It is not ready to merge until:

- GitHub CI reports a passing `CI Result`;
- an independent fresh-context reviewer returns a machine-parseable approval;
- valid findings are addressed and review threads are resolved;
- the final branch still passes the required formatting and lint gates.

The PR should remain explicit that Slice B establishes a safe execution unit, while Slice C enables workflow dispatch and semantic routing.
