# Technical Design: Minimal Workflow Classification Path

Status: Proposed — owner approval pending

Date: 2026-08-28

Source prototype: `origin/feat/generic-workflow-classifier` (`3ca7df2c`, `32dd03df`)

Related umbrella proposal: `docs/workflow-first-autonomous-change-rfc.md`

## 1. Objective

Deliver a model-based semantic classification activity through the current Workflow runtime and any
conforming `AgentBackend`, with Codex as the required dogfood runtime. Do this without waiting for a
vNext database cutover and without adding a classifier-specific branch to runtime orchestration.

This is a deliberately narrow vertical slice. It must produce reusable activity-policy,
structured-output, enforcement, and replay contracts that a future Workflow architecture can keep.

## 2. Current Facts

- The current server already exposes a lexical `complexity_router`; that is not the semantic
  Workflow classifier proposed here.
- The current `WorkflowActivityPolicy` contains only `prompt` and repository `validation` commands.
- The current runtime already pins declarative definition identity, persists runtime jobs, records
  runtime events, supports structured `ActivityResult`, and runs Codex through both oneshot and
  per-turn `AgentBackend` surfaces.
- PR #2010 proves a Codex-backed, no-tool classifier can execute, but its two commits touch 43 files
  with approximately 1,854 insertions and 429 deletions.
- PR #2010 also introduces a classifier-specific config type and cross-layer routing. That shape is
  not accepted wholesale.

## 3. Required User Outcome

Given a Workflow-declared semantic activity, Harness must:

1. collect and persist caller/server facts with provenance;
2. pin the activity contract into the runtime job;
3. select any configured `AgentBackend` satisfying the contract;
4. run one fresh, tool-free, non-mutating turn;
5. validate structured output against the pinned schema and verdict allowlist;
6. persist a server-authored assessment with attempt-wide enforcement observations;
7. route only through Workflow-declared verdict signals; and
8. replay completion without invoking the model again.

A Claude account is not required. The initial dogfood profile uses Codex with the configured model.

```mermaid
sequenceDiagram
    participant Workflow
    participant Runtime
    participant Backend as AgentBackend (Codex first)
    participant Store

    Workflow->>Runtime: Declared agent activity + input facts
    Runtime->>Store: Pin contract, facts, provenance, job identity
    Runtime->>Backend: Fresh no-tool structured-output turn
    Backend-->>Runtime: Candidate verdict + rationale + evidence refs
    Runtime->>Runtime: Validate schema, allowlist, model/tool/mutation observations
    Runtime->>Store: Persist server-authored assessment
    Runtime->>Workflow: Emit one declared verdict signal
    Note over Runtime,Store: Replay consumes persisted assessment; no second model call
```

## 4. Minimal Generic Contract

Extend the current activity policy with one generic agent-execution contract rather than a
`classifier` field:

```yaml
activities:
  classify_scope:
    prompt: Classify only the supplied facts.
    agent_contract:
      input_schema: harness.semantic_activity_input.v1
      output_schema: harness.semantic_verdict.v1
      allowed_outcomes: [small, medium, large, blocked]
      tools: none
      mutation: forbidden
      workspace: ephemeral_empty
      fresh_context: true
      max_primary_attempts: 1
      max_corrections: 1

definition:
  states:
    classifying:
      activity: classify_scope
      on_failure: blocked
      on_signal:
        small: implementing_small
        medium: planning
        large: decomposing
        blocked: blocked
```

The exact field names remain proposed, but these ownership rules are required:

- the Workflow declares schemas, allowed outcomes, prompt, and routes;
- the runtime enforces tools, mutation, workspace, freshness, attempts, and structured output;
- the selected backend and model live in runtime-profile configuration, never Workflow state logic;
- the input envelope contains only persisted facts and provenance, not mutable request reconstruction;
- the complete resolved contract participates in the pinned definition/job identity; and
- completion uses one server-authored assessment, not an agent-authored approval artifact.

This generic contract can later serve risk assessment, routing, review triage, and other bounded
semantic judgments. Core code must not match on activity name, workflow ID, provider, or
`RuntimeKind` to determine state transitions.

## 5. Input and Output

Minimum input envelope:

```json
{
  "schema": "harness.semantic_activity_input.v1",
  "subject": {"kind": "task", "identity": "submission:..."},
  "facts": {},
  "provenance": {},
  "contract_hash": "sha256:..."
}
```

Minimum model output:

```json
{
  "schema": "harness.semantic_verdict.v1",
  "outcome": "medium",
  "rationale": "...",
  "evidence_refs": ["/facts/changed_files"]
}
```

The server-authored assessment additionally binds:

- workflow, command, runtime job, and attempt identity;
- pinned definition and activity-contract hashes;
- input-fact digest;
- runtime profile and observed model identity/source;
- observed tool, approval, network, and mutation events across primary and correction attempts;
- output digest and accepted outcome; and
- final validation result.

Unknown fields, missing provenance, an outcome outside the allowlist, evidence references outside
the input envelope, any tool/mutation observation, unverifiable model identity when required, or a
contract/hash mismatch fail the activity explicitly.

## 6. Attempt and Correction Semantics

The initial implementation may use the current runtime job plus runtime-event stream instead of a
new vNext attempt table, provided it can reconstruct one immutable attempt assessment.

All observations from the primary turn and the optional structured-output correction turn are
folded together. A correction cannot erase a tool call, approval, mutation, or model mismatch from
the primary turn. The correction receives only the prior textual output and validation error; it has
the same no-tool/non-mutating contract.

This is the smallest forward-compatible bridge to a future typed `ActivityAttempt`. It must not
pretend the current persistence model already implements the full vNext Evidence system.

## 7. Scope and Explicit Exclusions

In scope:

- generic activity-policy declaration and validation;
- pinned input/contract snapshot;
- Code-Agent-neutral backend selection;
- Codex structured-output execution;
- attempt-wide enforcement observations;
- server-authored assessment;
- deterministic verdict routing and replay; and
- one Codex dogfood Workflow.

Out of scope:

- vNext schema epoch, database cutover, or legacy data handling;
- automatic merge, risk authorization, Child Work Items, or integration strategies;
- changes to `CreateTaskRequest` for classifier-specific input;
- classifier-specific `RuntimeKind`, dispatcher, reducer, or activity-name branches;
- model selection inside Workflow YAML;
- repository mutation, validation commands, or ordinary implementation workspaces; and
- merging PR #2010 wholesale.

## 8. Prototype Salvage Boundaries

Retain as concepts, after genericization and focused review:

- opaque fact/provenance input envelope;
- pinned policy/contract digest;
- exact outcome-route allowlist;
- server-authored assessment;
- Codex launch-derived model identity when the protocol cannot report one;
- removal of agent-authored routing signals before server validation;
- replay from persisted assessment; and
- attempt-wide tool/mutation invalidation.

Do not retain as architecture:

- `WorkflowClassifierPolicy` as a classifier-only core config surface;
- classifier-specific request/intake fields;
- classifier-specific workspace or executor branches;
- broad dispatcher refactors unrelated to the vertical slice;
- hard-coded Claude assumptions; or
- any fallback that treats missing enforcement data as success.

Every retained hunk must map to a requirement and focused test in this document. Commit ancestry is
not a reason to keep code.

## 9. Delivery Slices

### Slice A — Generic declaration and pinning

- add the generic `agent_contract` activity policy;
- validate schemas, allowed outcomes, no-tool/mutation/workspace constraints, and exact routes;
- include the resolved contract in definition identity and runtime-job snapshot; and
- add parser/compiler/pinning tests.

### Slice B — Backend enforcement and structured output

- pass the pinned output schema through `AgentBackend` capabilities;
- make Codex the first conforming runtime;
- capture model identity and attempt-wide tool/mutation observations; and
- fail closed when the backend cannot enforce the declared contract.

### Slice C — Assessment, routing, and replay

- validate one candidate output;
- persist one server-authored assessment;
- emit one allowed Workflow signal;
- replay without redispatch; and
- expose operator-visible failure reasons.

### Slice D — Dogfood

- run a real classification through the Workflow runtime submission API using Codex;
- prove operation with no Claude credentials;
- restart and replay the accepted assessment; and
- publish the evidence and measured latency/cost.

Each slice must be independently reviewable and must leave ordinary activities unchanged.

## 10. Alternatives

### A. Generic semantic activity on the current runtime — recommended

Pros: shortest reusable path; no cutover; Code-Agent-neutral; validates the abstraction before
vNext. Cons: current runtime events temporarily represent attempt evidence less strongly than the
future typed table.

### B. Wait for the complete vNext runtime

Pros: one final persistence model. Cons: delays the required classifier behind unrelated child,
merge, authorization, and cutover work; high risk of specification work without user value.

Decision: rejected as a prerequisite for classification.

### C. Merge PR #2010 after fixing individual review comments

Pros: existing implementation is close to dogfood. Cons: 43-file cross-layer patch retains
classifier-specific surfaces and makes architecture review difficult.

Decision: rejected wholesale; salvage only mapped generic pieces.

### D. Keep the lexical complexity router only

Pros: no model cost or runtime changes. Cons: cannot make repository/task-semantic judgments and
does not satisfy the requested Code Agent classification flow.

Decision: retained as a cheap separate heuristic, not a substitute.

## 11. Success Metrics

| Metric | Target | Evidence |
|---|---:|---|
| Codex-only end-to-end classifications | 100% of dogfood cases | Runtime submission and persisted assessment |
| Claude credential dependency | 0 | Sanitized dogfood environment |
| Undeclared outcome accepted | 0 | Contract tests |
| Tool/mutation observation accepted | 0 | Primary and correction-attempt tests |
| Model redispatch during replay | 0 | Restart/replay test |
| Classifier-specific state-machine branches | 0 | Static search and review |
| Ordinary activity regressions | 0 | Focused existing runtime tests |
| Silent fallback on missing enforcement evidence | 0 | Failure-path tests |

Latency and cost targets must be measured during dogfood rather than invented in advance.

## 12. Risks and Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| “Generic” contract secretly encodes classifier behavior | High | Reuse it for a second semantic fixture; prohibit activity-name/runtime-kind routing |
| Current event stream cannot prove attempt-wide enforcement | High | Add only the minimum immutable assessment/event fields; block dogfood if reconstruction is ambiguous |
| Codex model identity is launch-derived rather than provider-reported | Medium | Record identity source explicitly; require exact trusted launch argument |
| Correction turn erases a primary violation | Critical | Fold observations monotonically across all turns |
| Empty workspace prevents required facts | Medium | Persist facts before dispatch; no repository reads inside the classifier turn |
| Scope expands back toward PR #2010 | High | Requirement-to-file/test traceability and per-slice review |

## 13. Approval and Done Criteria

Implementation may begin only after the owner approves this narrow design and the proposed generic
activity field shape.

The feature is done only when:

- all four delivery slices pass focused tests;
- a fresh-context reviewer approves the final diff;
- a real Codex-only Workflow submission produces and replays a persisted assessment;
- no Claude credential is present in the dogfood environment;
- no classifier-specific core routing remains; and
- the implementation report lists every salvaged PR #2010 hunk and its requirement/test mapping.

Approval of this document does not approve the umbrella vNext RFC or the cutover RFC.
