# Technical Design: Minimal Workflow Classification Path

Status: Approved current-runtime path — Slices A and B merged; Slices C and D pending

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
- At the proposal baseline, `WorkflowActivityPolicy` contained only `prompt` and repository
  `validation` commands. PR #2020 added the strict generic `agent_contract` surface.
- The current runtime already pins declarative definition identity, persists runtime jobs, records
  runtime events, supports structured `ActivityResult`, and runs Codex through both oneshot and
  per-turn `AgentBackend` surfaces.
- Slice A merged in PR #2020: generic declaration, validation, persistence, and contract pinning.
- Slice B merged in PR #2025: constrained and observable `AgentBackend` execution. The production
  dispatcher remains fail-closed until Slice C supplies assessment, routing, budgets, and replay.
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
- the runtime validates the persisted provenance sidecar against the facts exactly once before
  accepting the pinned envelope, and every attempt consumes that validated envelope;
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
  "facts": {"changed_files": ["src/lib.rs"]},
  "provenance": {
    "schema": "harness.workflow.data_provenance.v1",
    "entries": {"/changed_files": "server"},
    "value_digests": {"/changed_files": "sha256:..."}
  },
  "contract_hash": "sha256:..."
}
```

`provenance` uses the existing `WorkflowDataProvenance` pointer, nearest-ancestor coverage, and
value-digest rules against the `facts` object. Empty facts may use an empty current-schema sidecar.
For non-empty facts, every leaf must have exactly one unambiguous exact-or-nearest-ancestor
classification, every pointer and digest must resolve, and orphan or `legacy_entries` coverage is
invalid for a semantic activity. This relationship is validated once when the runtime job accepts
the pinned envelope, before any backend dispatch; primary and correction attempts consume the same
opaque validated envelope and hash rather than repeating or bypassing that check.

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
- canonical digest covering the complete input envelope, including its schema, subject, facts,
  provenance, and contract hash;
- runtime profile and observed model identity/source;
- observed tool, approval, network, and mutation events across primary and correction attempts;
- output digest and accepted outcome; and
- final validation result.

Unknown fields, incomplete, orphaned, legacy, ambiguous, or digest-mismatched fact provenance, an
outcome outside the allowlist, evidence references outside a provenance-covered `/facts/...` path,
any model-initiated tool, approval, external-data/network, or mutation observation, unverifiable
model identity when required, or a contract/hash mismatch fail the activity explicitly. Provider
transport egress to the selected model endpoint is required to run the attempt and is not
model-visible network permission; `tools: none` forbids model-initiated web search or other network
tools.

## 6. Attempt and Correction Semantics

The initial implementation may use the current runtime job plus runtime-event stream instead of a
new vNext attempt table, provided it can reconstruct one immutable attempt assessment.

All observations from the primary turn and the optional structured-output correction turn are
folded together. A correction cannot erase a tool call, approval, mutation, or model mismatch from
the primary turn. Every correction is a fresh-context request containing the exact primary prompt,
the same pinned immutable input envelope and output schema, plus the prior raw output and the
server-authored structured validation error. It binds the primary attempt ID, correction ordinal,
and unchanged input-envelope hash; it never relies on backend conversation history or reconstructs
facts from current Workflow data. It has the same no-tool/non-mutating contract and does not refresh
facts, contract identity, or the correction budget.

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

Status: merged in PR #2020.

- add the generic `agent_contract` activity policy;
- validate schemas, allowed outcomes, no-tool/mutation/workspace constraints, and exact routes;
- include the resolved contract in definition identity and runtime-job snapshot; and
- add parser/compiler/pinning tests.

### Slice B — Backend enforcement and structured output

Status: merged in PR #2025. The dispatcher barrier remains closed by design.

- pass the pinned output schema through `AgentBackend` capabilities;
- make Codex the first conforming runtime;
- capture model identity and attempt-wide tool/mutation observations; and
- fail closed when the backend cannot enforce the declared contract.

### Slice C — Assessment, routing, and replay

Status: not implemented.

- validate one candidate output;
- persist one server-authored assessment;
- emit one allowed Workflow signal;
- replay without redispatch; and
- expose operator-visible failure reasons.

### Slice D — Dogfood

Status: not implemented.

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
| Invalid fact provenance accepted | 0 | Empty, partial, orphaned, legacy, ambiguous, and digest-mismatch boundary tests |
| Stateless correction missing pinned primary inputs | 0 | Correction request capture tests |
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
