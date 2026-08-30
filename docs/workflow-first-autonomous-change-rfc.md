# RFC: Workflow-First Autonomous Change Closure

Status: Proposed — machine-reviewed; owner approval pending

Date: 2026-08-28

Audience: Harness maintainers, workflow authors, runtime implementers, and reviewers

Decision record: `docs/workflow-first-autonomous-change-decisions.md`

Machine review record: two fresh-context agent reviews reported no blocking internal-consistency or
repository-fact findings. This is advisory evidence, not owner approval and not authorization to
implement or cut over production.

## Abstract

This RFC defines a complete Workflow-first architecture for taking an Issue, task, API submission,
or existing pull request through fact collection, risk assessment, optional decomposition,
implementation, independent review, CI verification, tiered authorization, merge, and terminal
reconciliation.

The design deliberately keeps the Harness core small. Core code owns facts, identity, persistence,
leases, authorization, evidence integrity, and transition safety. A versioned Workflow declares
the domain-specific states, activities, capability requirements, payload schemas, routing,
decomposition policy, review policy, retry policy, and merge policy. Agents provide judgment and
execution inside those contracts; they do not own workflow state or authority.

This RFC integrates and narrows several existing Harness designs. It does not authorize code
changes until the RFC is reviewed and approved.

## Normative Language

`MUST`, `MUST NOT`, `SHOULD`, `SHOULD NOT`, and `MAY` are normative as defined by RFC 2119.

## 1. Problem

Harness has strong runtime pieces, but architecture has evolved feature by feature. Intake,
declarative definitions, prompt contracts, classifier activities, workspace isolation, review,
remote fact collection, and merge authorization each have partial contracts. When a new feature
crosses these boundaries, local changes can create system-wide failures. The classifier prototype
in PR #2010 exposed this pattern: a useful semantic activity also affected workspace selection,
retry evidence, intake requirements, activity selection, and reducer routing.

The system needs one complete contract before more autonomous behavior is added. The contract must
answer:

- what the Workflow may declare dynamically;
- what the Harness core must always enforce;
- how arbitrary Code Agents participate without provider-specific workflow logic;
- how large changes are decomposed without turning every agent TODO into a scheduler node;
- how local and integration review remain independent;
- how risk limits execution and merge authority;
- how current GitHub facts and durable Harness history are reconciled; and
- how every failure becomes explicit, bounded, recoverable, and observable.

## 2. Goals

1. Provide one closed loop from intake to externally confirmed merge or an explicit terminal stop.
2. Keep workflow policy repository-owned, versioned, declarative, and extensible.
3. Keep the workflow runtime Code-Agent-neutral and model-neutral.
4. Separate deterministic facts from model judgment.
5. Support both task-first implementation and PR-first repair/review.
6. Support large-change decomposition without requiring a general-purpose live DAG engine.
7. Enforce risk-aware execution and merge authority.
8. Require child-level and parent-level independent review.
9. Preserve deterministic replay and safe recovery across restart.
10. Make every dispatch, retry, approval, review, and merge decision auditable.
11. Prevent silent degradation, stale approval reuse, evidence forgery, and unbounded agent loops.
12. Define a vNext runtime contract that never reads or reinterprets pre-vNext runs; evaluate the
    physical cutover separately from this umbrella architecture.

## 3. Non-Goals

- Model every internal agent planning step as a persisted scheduler node.
- Make agents deterministic.
- Encode project-specific risk judgment in Rust enums.
- Build a generic distributed compute scheduler.
- Require a particular model, provider, CLI, tracker, or review bot.
- Trust agent prose as proof of GitHub, CI, merge, or human-review state.
- Make vNext read or reinterpret any pre-vNext runtime data, definition, event, Evidence, or
  compatibility-only payload. Historical audit retention is owned by the separate cutover RFC.
- Migrate an in-flight Work Item to a different Workflow definition.
- Permit an in-flight Workflow definition to change implicitly.
- Add server-side `git` or `gh` subprocess calls inside Harness crates.
- Automatically merge repositories that have not opted into an eligible policy.

## 4. Design Principles

1. **Workflow declares policy.** States, activities, routes, schemas, capability requirements, and
   bounded operating policy belong in a versioned Workflow.
2. **Harness enforces invariants.** Identity, authorship, freshness, isolation, authorization,
   event ordering, and transition integrity are not delegated.
3. **Agents propose; reducers decide.** An Agent may emit evidence, a verdict, a plan, or a
   decomposition proposal. Only the workflow runtime commits state.
4. **Facts precede judgment.** Cheap machine facts are collected before an Agent is dispatched.
5. **No silent degradation.** Missing capabilities, malformed results, unavailable reviewers, and
   stale evidence produce explicit blocked or failed outcomes.
6. **Every loop is bounded.** Retries, repairs, review cycles, decomposition depth, and child count
   have persisted budgets.
7. **Active definitions are immutable.** Every run pins the exact Workflow content it executes.
8. **Review is evidence, not ceremony.** Approval is bound to reviewer identity, role, scope, and
   exact code identity.
9. **Merge is a fresh transaction.** The merge gate re-reads current remote facts immediately
   before merge.
10. **Extensibility uses schemas, not unchecked maps.** Domain payloads may evolve without making
    their producer, subject, version, or integrity ambiguous.

## 5. Alternatives Considered

### 5.1 Symphony-style Issue runner

One tracker Issue owns one workspace and one Agent-maintained internal plan. The orchestrator owns
claims, concurrency, retries, and reconciliation but does not interpret a child plan.

Benefits:

- small core;
- high Agent flexibility;
- operationally understandable; and
- proven useful for independent Issues.

Limitations:

- large changes remain one opaque execution unit unless manually pre-split;
- no native child-level ownership or review;
- no integration-level completeness proof; and
- an internal checklist cannot be independently scheduled or recovered.

Decision: adopt the durable-work-unit versus internal-plan distinction, but add validated promotion
of selected plan parts into Child Work Items.

References:

- <https://github.com/openai/symphony>
- <https://github.com/openai/symphony/blob/main/SPEC.md>
- <https://github.com/openai/symphony/blob/main/elixir/WORKFLOW.md>

### 5.2 Universal dynamic DAG

Every planning step becomes a typed node that the orchestrator understands and schedules.

Benefits:

- maximum parallelism and observability;
- precise node-level retries and budgets; and
- explicit dependency scheduling.

Limitations:

- hardens transient Agent reasoning into runtime schema;
- greatly expands graph mutation, migration, and consistency complexity;
- creates pressure to encode product judgment in the scheduler; and
- makes simple work expensive.

Decision: rejected as the default abstraction.

### 5.3 Layered planning with promoted Child Work Items

An Agent owns the internal plan for one Work Item. When a part needs independent scheduling,
isolation, parallelism, or acceptance, the Agent submits a typed decomposition proposal. Harness
validates and materializes durable children.

Benefits:

- preserves Agent flexibility;
- exposes only meaningful orchestration boundaries;
- supports independent workspaces and reviews; and
- bounds graph complexity.

Limitations:

- requires explicit promotion criteria;
- requires parent/child aggregation semantics; and
- cannot introspect every internal Agent step.

Decision: selected.

## 6. System Context

```mermaid
flowchart TD
    O[Operator or Repository Owner]
    S[Issue / PR / API Source]
    H[Harness Workflow Runtime]
    C[Code Agent Runtime]
    G[GitHub / Tracker / CI]
    P[(Workflow Event Store)]

    O -->|Workflow policy and approvals| H
    S -->|Normalized intake| H
    H -->|Activity contract and facts| C
    C -->|Candidate evidence, decisions, plans| H
    H -->|Read facts / execute authorized writes| G
    G -->|Current external facts| H
    H -->|Events, commands, receipts| P
    P -->|Replay| H
```

## 7. Container Architecture

```mermaid
flowchart TD
    IA[Ingress Adapters] --> N[Work Item Normalizer]
    N --> WR[Workflow Runtime]

    WL[Workflow Loader and Compiler] --> DR[Definition Registry]
    DR --> WR

    FC[Deterministic Fact Collectors] --> ES[Evidence Service]
    ES --> WR

    WR --> PG[Policy and Risk Gate]
    PG --> D[Dispatcher]
    D --> AR[Agent Runtime Adapter]
    AR --> X[Code Agent]
    X --> AR
    AR --> RV[Result Validator]
    RV --> WR

    WR --> DV[Decomposition Validator]
    DV --> CW[Child Work Item Materializer]
    CW --> WR

    WR --> RG[Review Gate]
    RG --> MG[Merge Gate]
    MG --> RP[Remote Provider Adapter]

    WR --> EV[(Event Log and Command Outbox)]
    EV --> RC[Reconciler]
    RP --> RC
    RC --> WR
```

### 7.1 Responsibility Summary

| Component | Owns | Must not own |
|---|---|---|
| Ingress adapter | Provider normalization and source identity | Workflow transitions |
| Workflow compiler | Definition validation, schema compilation, content hash | Runtime state |
| Fact collector | Deterministic external observations | Semantic verdicts |
| Policy/risk gate | Risk floor and authority ceiling | Agent implementation plan |
| Dispatcher | Capability matching, lease-safe job creation | Product routing judgment |
| Agent adapter | Protocol translation and verified execution properties | Workflow state |
| Result validator | Envelope, schema, authorship, and activity-contract validation | Inventing missing facts |
| Reducer | Deterministic state transition and command production | External side effects |
| Decomposition validator | Graph safety and proposal bounds | Creating unvalidated children |
| Review gate | Independence, scope, head, protocol, and quorum checks | Code authorship |
| Merge gate | Fresh remote predicates and authorization | Trusting cached merge state |
| Reconciler | External/internal divergence detection | Last-write-wins repair |

## 8. Thin Core Versus Workflow Policy

The six primary domain objects are not six hard-coded workflows. They are the minimum shared
language needed to identify work, interpret a versioned policy, execute an activity, trust evidence,
record a decision, and relate independently scheduled children.

### 8.1 Core invariants

Core code MUST enforce:

- stable identities and idempotency keys;
- immutable Workflow pinning;
- event ordering and atomic reducer commits;
- activity attempt leases and fencing;
- evidence producer identity and content hashes;
- required schema validation;
- capability requirement satisfaction;
- no author/reviewer identity overlap when independence is required;
- head/base freshness for code-bound evidence;
- bounded graph, retry, repair, and review loops;
- no merge without a fresh eligible authorization receipt; and
- explicit error outcomes instead of silent fallback.

### 8.2 Workflow-defined policy

A Workflow MAY declare:

- logical states and routes;
- activities and prompts;
- required runtime capabilities;
- input, output, signal, and evidence schemas;
- deterministic project risk rules above the universal floor;
- semantic classifier rubrics and verdicts;
- decomposition eligibility and limits;
- allowed integration strategies;
- review roles, quorum, and specialist requirements;
- validation commands;
- retry and backoff within server limits;
- execution and token budgets;
- approval points;
- merge eligibility up to the core safety floor; and
- provider-specific fact requirements through registered adapters.

## 9. Core Domain Model

### 9.1 `WorkItem`

`WorkItem` is the durable unit of user-visible intent. It does not assume whether code already
exists.

This is a logical domain object, not a requirement for a parallel persistence table. The intended
brownfield mapping is the existing `WorkflowInstance` plus its runtime projection and explicitly
typed supporting records. Implementation must extend that path rather than creating a second task
store.

```text
WorkItem
  work_item_id
  submission_id
  parent_work_item_id?
  source
  subject
  intent
  repository
  base_ref?
  observed_head_sha?
  remote_change_binding_id?
  workflow_definition_id
  workflow_definition_version
  workflow_definition_hash
  logical_state
  lifecycle_class
  risk_floor
  effective_risk
  execution_authority
  merge_authority
  integration_strategy?
  budget
  created_at
  updated_at
```

Required invariants:

- `work_item_id` is the workflow instance correlation identity.
- `submission_id` is the stable public submission handle.
- `source + subject` provides a provider-aware dedupe identity when applicable.
- `intent` is persisted and reconstructable after restart.
- the definition hash never changes in place.
- `effective_risk` never falls below the server universal or operator non-lowerable floor; lowering
  the project assessment requires a valid, bound human override receipt.
- logical state is interpreted only by the pinned Workflow.
- lifecycle class is one of `active`, `blocked`, `succeeded`, `failed`, or `cancelled` and provides
  a small universal projection without fixing the logical state graph.

### 9.2 `WorkflowDefinition`

`WorkflowDefinition` is an immutable compiled policy bundle.

```text
WorkflowDefinition
  definition_id
  schema_version
  semantic_version
  content_hash
  intake_bindings
  states
  terminal_mapping
  activities
  evidence_schemas
  risk_policy
  decomposition_policy
  integration_policy
  review_policy
  authorization_policy
  retry_policy
  budget_policy
  recovery_policy
```

Required invariants:

- the source document is repository-owned or explicitly operator-supplied;
- compilation either succeeds completely or the definition is unavailable for dispatch;
- all referenced states, activities, schemas, and routes exist;
- every active state has a progress mechanism;
- every route is reachable or explicitly reserved for failure/recovery;
- limits may be stricter than server limits, never looser;
- unknown required capabilities fail validation;
- semantic changes produce a different content hash; and
- active instances retain the compiled bundle needed for replay.

### 9.3 `Activity`

`Activity` is a Workflow-declared execution contract, not a Rust-specific implementation name.

```text
Activity
  activity_id
  purpose
  allowed_current_states
  execution_mode
  input_schema
  output_schema
  required_capabilities
  allowed_tools
  permission_policy
  required_evidence
  produced_evidence
  allowed_decisions
  idempotency_contract?
  authority_contract?
  reconciliation_contract?
  retry_policy
  repair_policy
  budget
  prompt_template
```

An `ActivityAttempt` binds the abstract activity to one execution:

```text
ActivityAttempt
  attempt_id
  activity_id
  work_item_id
  command_id
  runtime_job_id
  agent_run_id
  actor_assignment_id
  author_identity
  attempt_kind
  runtime_profile_snapshot
  capability_snapshot
  prompt_packet_hash
  input_evidence_ids
  attempt_number
  lease_generation
  status
  started_at
  completed_at?
```

Required invariants:

- the prompt packet and runtime profile are snapshotted before execution;
- all attempts, including structured-output correction attempts, contribute to enforcement evidence;
- a retry never erases tool use, approval, network, mutation, or model-identity observations from a
  previous attempt in the same activity execution;
- result validation occurs before the reducer sees a candidate decision; and
- a completed process is not equivalent to a successful activity.

### 9.4 `Evidence`

Evidence uses a trusted core envelope and a Workflow-defined payload.

```text
Evidence
  evidence_id
  envelope_schema
  evidence_kind
  payload_schema
  producer_id
  producer_class
  producer_role
  subject
  work_item_id
  workflow_definition_hash
  activity_attempt_id?
  code_identity?
  source_identity?
  observed_at
  expires_at?
  content_hash
  payload
```

`producer_class` is core-controlled and includes at least:

- `server_fact_collector`;
- `server_policy_engine`;
- `agent_author`;
- `agent_reviewer`;
- `human_operator`;
- `remote_provider`;
- `ci_system`; and
- `runtime_enforcement`.

An Agent MUST NOT choose its producer class. The runtime derives it from the authenticated attempt
and role assignment.

`code_identity` is a tagged union. `single` binds one repository, base ref, head SHA, and optional
tree/diff hash. `aggregate` binds the integration strategy, decomposition revision, canonical
ordered member list of `(work_item_id, single code identity)`, and an aggregate hash. Independent
sets use canonical Work Item ordering; stacks use landing order. `source_identity` binds external
facts to provider, object ID, observation cursor, and remote version when available.

Evidence is invalid when its schema is unknown, content hash mismatches, producer is unauthorized,
required code identity is absent, or freshness requirements are not met. Trusted Evidence payload
schemas are closed: validation rejects unknown fields and requires the exact fields declared by the
pinned schema version.

`ReviewReceipt` and `AuthorizationReceipt` are specialized, versioned Evidence payloads. Their
Evidence rows are the only authorization truth consumed by reducers and merge gates. Any dedicated
receipt projection is a rebuildable index keyed by `evidence_id`, not an independent authority.

### 9.5 `Decision`

`Decision` is a validated proposal to route, authorize, block, retry, decompose, or terminalize.

```text
Decision
  decision_id
  decision_kind
  work_item_id
  current_state
  proposed_state
  workflow_definition_hash
  policy_rule_id
  input_evidence_ids
  candidate_source
  validation_result
  authority_required
  authority_receipt_id?
  reason
  created_at
```

Agents MAY propose decisions permitted by an activity contract. Reducers MUST independently verify:

- the decision kind is allowed;
- the current state and attempt match;
- all required evidence exists and is valid;
- the proposed route exists;
- required authorization is present; and
- the decision cannot bypass a higher-priority deterministic gate.

Invalid candidate decisions become explicit contract failures. They never fall through to success.
Transition paths without an active production caller and focused contract test remain fail-closed.

### 9.6 `ChildWorkItem`

`ChildWorkItem` is a normal `WorkItem` with a persisted parent relation and delegated scope.

```text
ChildWorkItemRelation
  parent_work_item_id
  child_work_item_id
  decomposition_revision
  dependency_ids
  delegated_intent
  acceptance_criteria
  writable_scope
  forbidden_scope
  integration_strategy
  landing_owner
  completion_milestone
  remote_binding_required
  integration_order?
  required_output_evidence
  status
```

Children have their own workflow state, workspace, attempts, evidence, review receipts, and terminal
outcome. Parent completion consumes typed child outcomes rather than inferring completion from prose
or branch existence.

### 9.7 Supporting objects

The following supporting objects are required but are not separate business roots:

- `DecompositionProposal`: candidate children and graph revision proposed by an Agent.
- `ReviewReceipt`: validated reviewer identity, scope, verdict, findings, and code identity.
- `AuthorizationReceipt`: human or policy authority, scope, reason, expiry, and risk level.
- `RuntimeCapabilitySnapshot`: trusted effective capabilities and enforcement evidence for one
  attempt.
- `ActorAssignment`: immutable server-issued role, scope, author set, protocol, context generation,
  and permission grant used to authenticate an attempt.
- `RemoteFactSnapshot`: current external facts with subject and observation identity.
- `RemoteChangeBinding`: durable provider/repository/change-request identity, base/head references,
  publication idempotency key, current code identity, and reconciliation state.
- `ProviderIntakeFence`: immutable cutover boundary per provider, repository, and subject type,
  containing the maximum trustworthy monotonic identity, snapshot source/hash, cutover time, and
  whether automatic intake is enabled or disabled as unverifiable.
- `ExternalWait`: persisted wait identity, subject/fact cursor, refresh contract and command,
  backoff, deadline, budget reservation, and terminal route status.
- `OperatorGate`: persisted requested action, prerequisite identities, accepted receipt kind, and
  finite signal-to-route map for one operator-owned state.
- `IntegrationProgress`: validated strategy/revision, ordered remote bindings, fenced landing cursor,
  landed entries, and current code identities.
- `ChildOutcome`: typed terminal result returned to the parent.

`Budget` is a supporting value object attached to a Work Item and allocated to attempts and
children. It records limits, reservations, and consumption for turns, tokens, wall time, retries,
repairs, review cycles, child count, and decomposition depth. A child reservation reduces the
parent's available budget atomically; unused reservation returns only through an explicit child
terminal event.

## 10. Workflow Declaration Model

The following is the normative reference fixture for the proposed ownership boundary. Owner
approval freezes this fixture as the Phase 2 compiler's conformance input; Phase 2 cannot complete
until its machine-readable schema and production compiler accept the extracted fixture unchanged.
A field rename then requires updating the schema, fixture, and this document together. Approval of
this proposal does not claim that the unimplemented compiler already exists.

```yaml
schema: harness.workflow.vNext

definition:
  id: autonomous_change
  version: 1
  initial: collecting_facts

  states:
    collecting_facts:
      activity: collect_change_facts
      on_signal:
        implementation_required: assessing_risk
        repair_required: assessing_risk
        review_ready: assessing_review_risk
      on_failure: blocked
    assessing_risk:
      activity: assess_risk
      on_signal:
        low: planning
        medium: planning
        high: planning
        abstain: blocked
      on_failure: blocked
    assessing_review_risk:
      activity: assess_risk
      on_signal:
        low: leaf_review_direct
        medium: leaf_review_direct
        high: leaf_review_direct
        abstain: blocked
      on_failure: blocked
    planning:
      activity: plan_change
      on_signal:
        direct: authorizing_direct
        decompose: validating_decomposition
        abstain: blocked
    validating_decomposition:
      activity: validate_decomposition
      on_success: authorizing_children
      on_failure: blocked
    authorizing_direct:
      activity: evaluate_execution_authority
      on_signal:
        authorized: implementing
        await_human: awaiting_direct_execution_authorization
        deny: blocked
    authorizing_children:
      activity: evaluate_execution_authority
      on_signal:
        authorized: materializing_children
        await_human: awaiting_child_execution_authorization
        deny: blocked
    awaiting_direct_execution_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: authorize_direct_execution
      on_signal:
        authorized: implementing
        expired: authorizing_direct
        denied: blocked
        cancelled: cancelled
    awaiting_child_execution_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: authorize_child_execution
      on_signal:
        authorized: materializing_children
        expired: authorizing_children
        denied: blocked
        cancelled: cancelled
    materializing_children:
      activity: materialize_children
      on_success: executing_children
      on_failure: blocked
    executing_children:
      progress: child_barrier
      on_signal:
        independent_ready: preparing_independent_set_review
        stacked_ready: preparing_stack_review
        integration_ready: integrating
        child_failed: blocked
    preparing_independent_set_review:
      activity: materialize_parent_review_subject
      on_success: reviewing_independent_set
      on_failure: blocked
    reviewing_independent_set:
      activity: review_independent_set
      on_signal:
        approved: releasing_independent_children
        changes_requested: planning
        blocked: blocked
    preparing_stack_review:
      activity: materialize_parent_review_subject
      on_success: reviewing_stack
      on_failure: blocked
    releasing_independent_children:
      activity: release_independent_children
      on_success: awaiting_independent_landing
      on_failure: blocked
    awaiting_independent_landing:
      progress: child_barrier
      on_signal:
        all_landed: reconciling_child_set
        child_review_stale: awaiting_independent_re_reviews
        child_failed: blocked
    awaiting_independent_re_reviews:
      progress: child_barrier
      on_signal:
        reviews_current: preparing_independent_set_review
        child_failed: blocked
    reconciling_child_set:
      activity: reconcile_child_set
      on_success: done
      on_failure: blocked
    reviewing_stack:
      activity: review_stack
      on_signal:
        approved: stack_merge_gate
        changes_requested: planning
        blocked: blocked
    integrating:
      activity: integrate_children
      on_success: publishing_integrated_change
      on_failure: blocked
    implementing:
      activity: implement_change
      on_success: routing_implemented_change
      on_failure: blocked
    routing_implemented_change:
      activity: select_landing_path
      on_signal:
        direct: publishing_direct_change
        independent_child: publishing_child_change
        stacked_child: publishing_child_change
        integration_child: leaf_review_child
        invalid: blocked
    publishing_direct_change:
      activity: publish_or_bind_change
      on_success: leaf_review_direct
      on_failure: blocked
    publishing_child_change:
      activity: publish_or_bind_change
      on_success: leaf_review_child
      on_failure: blocked
    publishing_integrated_change:
      activity: publish_or_bind_change
      on_success: integration_review
      on_failure: blocked
    leaf_review_direct:
      activity: review_change
      on_signal:
        approved: merge_gate
        changes_requested: collecting_facts
        blocked: blocked
      on_failure: blocked
    leaf_review_child:
      activity: review_change
      on_signal:
        approved: awaiting_parent_handoff
        changes_requested: implementing
        blocked: blocked
      on_failure: blocked
    awaiting_parent_handoff:
      progress: parent_handoff
      on_signal:
        release_independent: merge_gate
        stack_entry_landed: reconciling
        integration_contribution_accepted: done
        re_review_required: leaf_review_child
        parent_failed: blocked
    integration_review:
      progress: review_barrier
      review:
        activity: review_integration
        distinct_assignments: true
        quorum_policy: integration
      on_signal:
        approved: merge_gate
        changes_requested: integrating
        blocked: blocked
      on_failure: blocked
    merge_gate:
      activity: evaluate_merge_gate
      on_signal:
        auto_merge: merging
        await_human: awaiting_merge_authorization
        checks_pending: awaiting_remote_checks
        facts_unavailable: awaiting_remote_facts
        direct_review_stale: refreshing_direct_review_facts
        integration_review_stale: refreshing_integration_review_facts
        independent_child_review_stale: refreshing_independent_child_review_facts
        checks_failed: blocked
        deny: blocked
    awaiting_remote_checks:
      progress: external_wait
      wait:
        fact_kind: required_checks
        refresh_contract: registered.change_fact_collection.v1
        max_refreshes: 20
        deadline: 2h
        backoff: {initial: 15s, maximum: 5m, multiplier: 2}
      on_signal:
        fact_changed: merge_gate
        remote_failed: blocked
        deadline_exceeded: blocked
        budget_exhausted: blocked
    awaiting_remote_facts:
      progress: external_wait
      wait:
        fact_kind: remote_subject
        refresh_contract: registered.change_fact_collection.v1
        max_refreshes: 12
        deadline: 1h
        backoff: {initial: 15s, maximum: 5m, multiplier: 2}
      on_signal:
        fact_changed: merge_gate
        remote_failed: blocked
        deadline_exceeded: blocked
        budget_exhausted: blocked
    refreshing_direct_review_facts:
      activity: collect_change_facts
      on_success: assessing_review_risk
      on_failure: blocked
    refreshing_integration_review_facts:
      activity: collect_change_facts
      on_success: assessing_refreshed_integration_risk
      on_failure: blocked
    assessing_refreshed_integration_risk:
      activity: assess_risk
      on_signal:
        low: validating_refreshed_integration
        medium: validating_refreshed_integration
        high: validating_refreshed_integration
        abstain: blocked
      on_failure: blocked
    validating_refreshed_integration:
      activity: validate_integration_head
      on_success: integration_review
      on_failure: blocked
    refreshing_independent_child_review_facts:
      activity: collect_change_facts
      on_success: requesting_independent_child_review
      on_failure: blocked
    requesting_independent_child_review:
      activity: request_independent_child_review
      on_success: leaf_review_child
      on_failure: blocked
    awaiting_merge_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: merge_current_subject
      on_signal:
        authorized: merge_gate
        expired: merge_gate
        denied: blocked
    stack_merge_gate:
      activity: evaluate_stack_entry_gate
      on_signal:
        auto_merge: landing_stack_entry
        await_human: awaiting_stack_merge_authorization
        checks_pending: awaiting_stack_checks
        facts_unavailable: awaiting_stack_facts
        review_stale: refreshing_stale_stack_facts
        rebase_required: stack_rebase_gate
        checks_failed: blocked
        stack_complete: reconciling_child_set
        deny: blocked
    awaiting_stack_merge_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: merge_current_stack_entry
      on_signal:
        authorized: stack_merge_gate
        expired: stack_merge_gate
        denied: blocked
    awaiting_stack_checks:
      progress: external_wait
      wait:
        fact_kind: required_checks
        refresh_contract: registered.change_fact_collection.v1
        max_refreshes: 20
        deadline: 2h
        backoff: {initial: 15s, maximum: 5m, multiplier: 2}
      on_signal:
        fact_changed: stack_merge_gate
        remote_failed: blocked
        deadline_exceeded: blocked
        budget_exhausted: blocked
    awaiting_stack_facts:
      progress: external_wait
      wait:
        fact_kind: remote_subject
        refresh_contract: registered.change_fact_collection.v1
        max_refreshes: 12
        deadline: 1h
        backoff: {initial: 15s, maximum: 5m, multiplier: 2}
      on_signal:
        fact_changed: stack_merge_gate
        remote_failed: blocked
        deadline_exceeded: blocked
        budget_exhausted: blocked
    refreshing_stale_stack_facts:
      activity: collect_change_facts
      on_success: requesting_stack_child_reviews
      on_failure: blocked
    landing_stack_entry:
      activity: merge_stack_entry
      on_success: reconciling_stack_entry
      on_failure: blocked
    reconciling_stack_entry:
      activity: reconcile_stack_entry
      on_signal:
        more_entries: stack_rebase_gate
        stack_complete: reconciling_child_set
        divergence: blocked
      on_failure: blocked
    stack_rebase_gate:
      activity: evaluate_stack_rebase_authority
      on_signal:
        authorized: rebasing_stack
        await_human: awaiting_stack_rebase_authorization
        deny: blocked
    awaiting_stack_rebase_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: rebase_remaining_stack
      on_signal:
        authorized: stack_rebase_gate
        expired: stack_rebase_gate
        denied: blocked
    rebasing_stack:
      activity: rebase_remaining_stack
      on_success: stack_republication_gate
      on_failure: blocked
    stack_republication_gate:
      activity: evaluate_stack_republication_authority
      on_signal:
        authorized: publishing_rebased_stack
        await_human: awaiting_stack_republication_authorization
        deny: blocked
    awaiting_stack_republication_authorization:
      progress: operator_gate
      gate:
        evidence_kind: authorization_receipt
        requested_action: republish_rebased_stack
      on_signal:
        authorized: stack_republication_gate
        expired: stack_republication_gate
        denied: blocked
    publishing_rebased_stack:
      activity: publish_rebased_stack
      on_success: refreshing_rebased_stack_facts
      on_failure: blocked
    refreshing_rebased_stack_facts:
      activity: collect_change_facts
      on_success: requesting_stack_child_reviews
      on_failure: blocked
    requesting_stack_child_reviews:
      activity: request_stack_child_reviews
      on_success: awaiting_stack_child_reviews
      on_failure: blocked
    awaiting_stack_child_reviews:
      progress: child_barrier
      on_signal:
        reviews_current: preparing_stack_review
        child_failed: blocked
      on_failure: blocked
    merging:
      activity: merge_change
      on_success: reconciling
      on_failure: blocked
    reconciling:
      activity: reconcile_remote_state
      on_success: done
      on_failure: blocked
    blocked:
      progress: operator_gate
      gate:
        evidence_kind: operator_recovery_receipt
        requested_action: recover_blocked_work_item
      on_signal:
        replan: collecting_facts
        cancel: cancelled

  recovery_targets:
    - collecting_facts

  terminal:
    done: succeeded
    failed: failed
    cancelled: cancelled

activities:
  collect_change_facts:
    executor: registered_server
    contract: registered.change_fact_collection.v1
    produces: [intake_subject_snapshot, code_change_snapshot, review_subject_snapshot]

  assess_risk:
    executor: agent
    requires:
      structured_output: true
      filesystem: none
      network: none
      tools: forbidden
    input_schema: semantic_risk_input.v1
    output_schema: semantic_risk_output.v1
    required_evidence: [intake_subject_snapshot]
    allowed_decisions: [low, medium, high, abstain]
    produces: [semantic_risk_assessment]

  plan_change:
    executor: agent
    requires:
      structured_output: true
      filesystem: read_only
      network: none
      tools: [read]
    input_schema: change_plan_input.v1
    output_schema: change_plan_output.v1
    required_evidence: [intake_subject_snapshot, semantic_risk_assessment]
    allowed_decisions: [direct, decompose, abstain]
    produces: [implementation_plan, decomposition_proposal]
    retry:
      max_attempts: 2

  validate_decomposition:
    executor: registered_server
    contract: registered.decomposition_validation.v1
    produces: [decomposition_validation]

  materialize_children:
    executor: registered_server
    contract: registered.decomposition_materialization.v1
    produces: [child_materialization]

  materialize_parent_review_subject:
    executor: registered_server
    contract: registered.parent_review_subject_materialization.v1
    required_evidence: [child_materialization, child_outcome]
    produces: [review_subject_snapshot]

  evaluate_execution_authority:
    executor: registered_server
    contract: registered.execution_authority_gate.v1
    produces: [authorization_gate_result, authorization_receipt]

  implement_change:
    executor: agent
    requires:
      structured_output: true
      filesystem: isolated_write
      network: none
      cancellation: true
    permissions:
      tools: [read, edit, command]
      provider_actions: []
      writable_scope: plan_authorized
    input_schema: change_implementation_input.v1
    output_schema: change_implementation_output.v1
    required_evidence: [implementation_plan, authorization_gate_result]
    allowed_decisions: []
    produces: [change_set, validation_report]

  select_landing_path:
    executor: registered_server
    contract: registered.landing_path_selection.v1
    produces: [landing_path_selection, review_subject_snapshot]

  review_change:
    executor: agent
    role: independent_reviewer
    requires:
      structured_output: true
      filesystem: read_only
      network: none
      fresh_context: true
    permissions: {tools: [read], provider_actions: []}
    input_schema: code_review_input.v1
    output_schema: code_review_output.v1
    required_evidence: [review_subject_snapshot]
    allowed_decisions: [approved, changes_requested, blocked]
    produces: [review_receipt]

  integrate_children:
    executor: agent
    requires:
      structured_output: true
      filesystem: isolated_write
      network: none
      cancellation: true
    permissions:
      tools: [read, edit, command]
      provider_actions: []
      writable_scope: plan_authorized
    input_schema: child_integration_input.v1
    output_schema: child_integration_output.v1
    required_evidence: [decomposition_validation, child_materialization, child_outcome, authorization_gate_result]
    allowed_decisions: []
    produces: [integrated_change_set, validation_report]

  review_integration:
    executor: agent
    role: independent_reviewer
    requires:
      structured_output: true
      filesystem: read_only
      network: none
      fresh_context: true
    permissions: {tools: [read], provider_actions: []}
    input_schema: integration_review_input.v1
    output_schema: code_review_output.v1
    required_evidence: [integrated_change_set, validation_report, review_subject_snapshot, semantic_risk_assessment]
    allowed_decisions: [approved, changes_requested, blocked]
    produces: [integration_review_receipt]

  validate_integration_head:
    executor: registered_server
    contract: registered.integration_head_validation.v1
    required_evidence: [integrated_change_set, remote_change_binding, review_subject_snapshot, semantic_risk_assessment]
    produces: [validation_report]

  review_independent_set:
    executor: agent
    role: independent_reviewer
    requires: {structured_output: true, filesystem: read_only, network: none, fresh_context: true}
    permissions: {tools: [read], provider_actions: []}
    input_schema: child_set_review_input.v1
    output_schema: code_review_output.v1
    required_evidence: [child_materialization, child_outcome, review_subject_snapshot]
    allowed_decisions: [approved, changes_requested, blocked]
    produces: [independent_set_review_receipt]

  review_stack:
    executor: agent
    role: independent_reviewer
    requires: {structured_output: true, filesystem: read_only, network: none, fresh_context: true}
    permissions: {tools: [read], provider_actions: []}
    input_schema: stack_review_input.v1
    output_schema: code_review_output.v1
    required_evidence: [child_materialization, child_outcome, review_subject_snapshot]
    allowed_decisions: [approved, changes_requested, blocked]
    produces: [stack_review_receipt]

  release_independent_children:
    executor: registered_server
    contract: registered.parent_handoff_release.v1
    produces: [parent_handoff_receipt]

  reconcile_child_set:
    executor: registered_server
    contract: registered.child_set_reconciliation.v1
    produces: [child_set_reconciliation]

  publish_or_bind_change:
    executor: provider_action
    contract: registered.provider_change_publish_or_bind.v1
    idempotency: required
    authority: execution_authorization
    reconciliation: registered.remote_binding_reconciliation.v1
    produces: [remote_change_binding, review_subject_snapshot]

  evaluate_merge_gate:
    executor: registered_server
    contract: registered.merge_gate.v1
    produces: [merge_gate_result, authorization_receipt]

  merge_change:
    executor: provider_action
    contract: registered.provider_merge.v1
    idempotency: required
    authority: merge_authorization
    reconciliation: registered.remote_reconciliation.v1
    produces: [merge_attempt_receipt]

  evaluate_stack_entry_gate:
    executor: registered_server
    contract: registered.stack_entry_merge_gate.v1
    produces: [merge_gate_result, stack_rebase_context, authorization_receipt]

  merge_stack_entry:
    executor: provider_action
    contract: registered.provider_stack_entry_merge.v1
    idempotency: required
    authority: stack_entry_merge_authorization
    reconciliation: registered.stack_entry_reconciliation.v1
    produces: [merge_attempt_receipt]

  reconcile_stack_entry:
    executor: registered_server
    contract: registered.stack_entry_reconciliation.v1
    produces: [stack_entry_reconciliation, stack_rebase_context]

  evaluate_stack_rebase_authority:
    executor: registered_server
    contract: registered.stack_rebase_authority_gate.v1
    produces: [authorization_gate_result, authorization_receipt]

  rebase_remaining_stack:
    executor: agent
    requires:
      structured_output: true
      filesystem: isolated_write
      network: none
      cancellation: true
    permissions:
      tools: [read, edit, command]
      provider_actions: []
      writable_scope: plan_authorized
    input_schema: stack_rebase_input.v1
    output_schema: stack_rebase_output.v1
    required_evidence: [stack_rebase_context, authorization_receipt]
    allowed_decisions: []
    authority: stack_rebase_authorization
    produces: [change_set, validation_report]

  evaluate_stack_republication_authority:
    executor: registered_server
    contract: registered.stack_republication_authority_gate.v1
    produces: [authorization_gate_result, authorization_receipt]

  publish_rebased_stack:
    executor: provider_action
    contract: registered.provider_stack_republish.v1
    idempotency: required
    authority: stack_republication_authorization
    reconciliation: registered.remote_binding_reconciliation.v1
    produces: [stack_republication_receipt, remote_change_binding]

  request_stack_child_reviews:
    executor: registered_server
    contract: registered.stack_child_review_refresh.v1
    required_evidence: [child_materialization, child_outcome, remote_change_binding, review_subject_snapshot]
    produces: [child_review_refresh, review_subject_snapshot]

  request_independent_child_review:
    executor: registered_server
    contract: registered.independent_child_review_refresh.v1
    required_evidence: [child_materialization, child_outcome, remote_change_binding, review_subject_snapshot]
    produces: [child_review_refresh, review_subject_snapshot]

  reconcile_remote_state:
    executor: registered_server
    contract: registered.remote_reconciliation.v1
    produces: [remote_merge_confirmation]

evidence:
  implementation_plan:
    payload_schema: .harness/schemas/implementation-plan.v1.json
    allowed_producers: [agent_author]
  decomposition_proposal:
    payload_schema: .harness/schemas/decomposition-proposal.v1.json
    allowed_producers: [agent_author]
  semantic_risk_assessment:
    payload_schema: .harness/schemas/semantic-risk-assessment.v1.json
    allowed_producers: [agent_author]
  change_set:
    payload_schema: .harness/schemas/change-set.v1.json
    allowed_producers: [agent_author]
  validation_report:
    payload_schema: .harness/schemas/validation-report.v1.json
    allowed_producers: [agent_author, server_policy_engine]
  integrated_change_set:
    payload_schema: .harness/schemas/integrated-change-set.v1.json
    allowed_producers: [agent_author]
  code_change_snapshot:
    payload_schema: .harness/schemas/code-change-snapshot.v1.json
    allowed_producers: [server_fact_collector]
  review_subject_snapshot:
    payload_schema: .harness/schemas/review-subject-snapshot.v1.json
    allowed_producers: [server_fact_collector, server_policy_engine, remote_provider]
  intake_subject_snapshot:
    payload_schema: .harness/schemas/intake-subject-snapshot.v1.json
    allowed_producers: [server_fact_collector]
  decomposition_validation:
    payload_schema: .harness/schemas/decomposition-validation.v1.json
    allowed_producers: [server_policy_engine]
  child_materialization:
    payload_schema: .harness/schemas/child-materialization.v1.json
    allowed_producers: [server_policy_engine]
  child_outcome:
    payload_schema: .harness/schemas/child-outcome.v1.json
    allowed_producers: [runtime_enforcement]
  authorization_gate_result:
    payload_schema: .harness/schemas/authorization-gate-result.v1.json
    allowed_producers: [server_policy_engine]
  authorization_receipt:
    payload_schema: .harness/schemas/authorization-receipt.v1.json
    allowed_producers: [server_policy_engine, human_operator]
  operator_recovery_receipt:
    payload_schema: .harness/schemas/operator-recovery-receipt.v1.json
    allowed_producers: [human_operator]
  landing_path_selection:
    payload_schema: .harness/schemas/landing-path-selection.v1.json
    allowed_producers: [server_policy_engine]
  review_receipt:
    payload_schema: .harness/schemas/review-receipt.v1.json
    allowed_producers: [agent_reviewer, human_operator]
  integration_review_receipt:
    payload_schema: .harness/schemas/review-receipt.v1.json
    allowed_producers: [agent_reviewer, human_operator]
  independent_set_review_receipt:
    payload_schema: .harness/schemas/review-receipt.v1.json
    allowed_producers: [agent_reviewer, human_operator]
  stack_review_receipt:
    payload_schema: .harness/schemas/review-receipt.v1.json
    allowed_producers: [agent_reviewer, human_operator]
  parent_handoff_receipt:
    payload_schema: .harness/schemas/parent-handoff-receipt.v1.json
    allowed_producers: [server_policy_engine]
  child_set_reconciliation:
    payload_schema: .harness/schemas/child-set-reconciliation.v1.json
    allowed_producers: [server_policy_engine]
  stack_entry_reconciliation:
    payload_schema: .harness/schemas/stack-entry-reconciliation.v1.json
    allowed_producers: [server_policy_engine]
  stack_rebase_context:
    payload_schema: .harness/schemas/stack-rebase-context.v1.json
    allowed_producers: [server_policy_engine]
  stack_republication_receipt:
    payload_schema: .harness/schemas/stack-republication-receipt.v1.json
    allowed_producers: [remote_provider]
  child_review_refresh:
    payload_schema: .harness/schemas/child-review-refresh.v1.json
    allowed_producers: [server_policy_engine]
  remote_change_binding:
    payload_schema: .harness/schemas/remote-change-binding.v1.json
    allowed_producers: [remote_provider]
  merge_gate_result:
    payload_schema: .harness/schemas/merge-gate-result.v1.json
    allowed_producers: [server_policy_engine]
  merge_attempt_receipt:
    payload_schema: .harness/schemas/merge-attempt-receipt.v1.json
    allowed_producers: [remote_provider]
  remote_merge_confirmation:
    payload_schema: .harness/schemas/remote-merge-confirmation.v1.json
    allowed_producers: [server_fact_collector]

risk:
  floor_rules:
    - id: protected_paths
      when:
        changed_paths_any: ["auth/**", "migrations/**", ".github/**"]
      at_least: high
  semantic_activity: assess_risk
  classifier_may_lower_floor: false

decomposition:
  enabled: true
  max_depth: 2
  max_children_per_revision: 8
  max_total_children: 20
  require_acceptance_coverage: true
  require_non_overlapping_write_scopes: true
  allowed_integration_strategies:
    - independent_prs
    - stacked_prs
    - integration_pr
  strategy_profiles:
    independent_prs:
      landing_path_signal: independent_child
      child_review_target: awaiting_parent_handoff
      landing_owner: child
      parent_release_signal: release_independent
      parent_completion: all_remote_merges_confirmed
    stacked_prs:
      landing_path_signal: stacked_child
      child_review_target: awaiting_parent_handoff
      landing_owner: parent
      parent_completion_signal: stack_entry_landed
      parent_merge_subject: ordered_child_bindings
      parent_completion: stack_remote_merges_confirmed
    integration_pr:
      landing_path_signal: integration_child
      child_review_target: awaiting_parent_handoff
      landing_owner: parent
      parent_completion_signal: integration_contribution_accepted
      parent_merge_subject: integrated_change_binding
      parent_completion: integration_remote_merge_confirmed

review:
  child:
    fresh_context: true
    author_reviewer_separation: true
    quorum: 1
  integration:
    fresh_context: true
    author_reviewer_separation: true
    quorum_by_risk:
      low: 1
      medium: 1
      high: 2

authorization:
  execution:
    low: automatic
    medium: automatic
    high: human
  merge:
    low: automatic
    medium: human
    high: human_only
```

### 10.1 Declaration versus reality

A Workflow may require `filesystem: read_only`; it does not make an Agent read-only by saying so.
The dispatcher MUST select a runtime profile that can enforce the requirement and persist the
effective enforcement snapshot. If no eligible runtime exists, dispatch fails explicitly.

Workflow declarations are policy input. Trusted runtime observations prove effective behavior.

## 11. Standard Closed-Loop Profile

The core does not require the following logical state names. The standard
`autonomous_change.v1` Workflow profile SHOULD use them for shared tooling and dashboards:

```mermaid
stateDiagram-v2
    [*] --> collecting_facts
    collecting_facts --> assessing_risk: implementation or repair required
    collecting_facts --> assessing_review_risk: existing PR review-ready
    assessing_risk --> planning
    assessing_review_risk --> leaf_review_direct: risk classified
    planning --> authorizing_direct: direct plan
    planning --> validating_decomposition: decomposition proposed
    validating_decomposition --> authorizing_children: valid
    authorizing_direct --> implementing: execution authorized
    authorizing_children --> materializing_children: execution authorized
    authorizing_direct --> awaiting_direct_execution_authorization: human required
    authorizing_children --> awaiting_child_execution_authorization: human required
    awaiting_direct_execution_authorization --> implementing: direct plan approved
    awaiting_child_execution_authorization --> materializing_children: child plan approved
    materializing_children --> executing_children: batch committed
    executing_children --> preparing_independent_set_review: independent children ready
    preparing_independent_set_review --> reviewing_independent_set: aggregate identity materialized
    executing_children --> preparing_stack_review: stack ready
    preparing_stack_review --> reviewing_stack: ordered identity materialized
    executing_children --> integrating: integration inputs ready
    reviewing_independent_set --> releasing_independent_children: approved
    reviewing_independent_set --> planning: changes requested
    releasing_independent_children --> awaiting_independent_landing
    awaiting_independent_landing --> reconciling_child_set: all remotely merged
    awaiting_independent_landing --> awaiting_independent_re_reviews: child review stale
    awaiting_independent_re_reviews --> preparing_independent_set_review: child reviews current
    reconciling_child_set --> done
    reviewing_stack --> stack_merge_gate: approved
    reviewing_stack --> planning: changes requested
    stack_merge_gate --> landing_stack_entry: entry eligible
    stack_merge_gate --> awaiting_stack_merge_authorization: human required
    stack_merge_gate --> awaiting_stack_checks: checks pending
    stack_merge_gate --> awaiting_stack_facts: facts unavailable
    stack_merge_gate --> refreshing_stale_stack_facts: review stale
    stack_merge_gate --> stack_rebase_gate: rebase required
    refreshing_stale_stack_facts --> requesting_stack_child_reviews: current identities collected
    landing_stack_entry --> reconciling_stack_entry
    reconciling_stack_entry --> stack_rebase_gate: more entries
    reconciling_stack_entry --> reconciling_child_set: stack complete
    stack_rebase_gate --> rebasing_stack: authorized
    stack_rebase_gate --> awaiting_stack_rebase_authorization: human required
    awaiting_stack_rebase_authorization --> stack_rebase_gate: authorization received
    rebasing_stack --> stack_republication_gate: local identities changed
    stack_republication_gate --> publishing_rebased_stack: authorized for new identity
    stack_republication_gate --> awaiting_stack_republication_authorization: human required
    awaiting_stack_republication_authorization --> stack_republication_gate: authorization received
    publishing_rebased_stack --> refreshing_rebased_stack_facts: bindings superseded
    refreshing_rebased_stack_facts --> requesting_stack_child_reviews: remote identities confirmed
    requesting_stack_child_reviews --> awaiting_stack_child_reviews: child commands committed
    awaiting_stack_child_reviews --> preparing_stack_review: all leaf reviews current
    awaiting_parent_handoff --> leaf_review_child: re-review required
    implementing --> routing_implemented_change
    routing_implemented_change --> publishing_direct_change: direct
    routing_implemented_change --> publishing_child_change: independent or stacked child
    routing_implemented_change --> leaf_review_child: integration child
    publishing_direct_change --> leaf_review_direct: bound
    publishing_child_change --> leaf_review_child: bound
    leaf_review_direct --> collecting_facts: changes requested
    leaf_review_direct --> merge_gate: approved
    leaf_review_child --> implementing: changes requested
    leaf_review_child --> awaiting_parent_handoff: approved
    awaiting_parent_handoff --> merge_gate: independent released
    awaiting_parent_handoff --> reconciling: stack entry landed
    awaiting_parent_handoff --> done: integration contribution accepted
    integrating --> publishing_integrated_change
    publishing_integrated_change --> integration_review: bound
    integration_review --> integrating: changes requested
    integration_review --> merge_gate: approved
    merge_gate --> merging: low risk auto-authorized
    merge_gate --> awaiting_merge_authorization: medium/high
    merge_gate --> awaiting_remote_checks: checks pending
    merge_gate --> awaiting_remote_facts: facts unavailable
    merge_gate --> refreshing_direct_review_facts: direct review stale
    merge_gate --> refreshing_integration_review_facts: integration review stale
    merge_gate --> refreshing_independent_child_review_facts: child review stale
    merge_gate --> blocked: checks failed or denied
    awaiting_remote_checks --> merge_gate: fact changed
    awaiting_remote_facts --> merge_gate: fact refresh
    refreshing_direct_review_facts --> assessing_review_risk: current identity collected
    refreshing_integration_review_facts --> assessing_refreshed_integration_risk: current identity collected
    assessing_refreshed_integration_risk --> validating_refreshed_integration: risk classified
    validating_refreshed_integration --> integration_review: current head validated
    refreshing_independent_child_review_facts --> requesting_independent_child_review: current identity collected
    requesting_independent_child_review --> leaf_review_child: parent notified
    awaiting_merge_authorization --> merge_gate: authorization received; refresh facts
    awaiting_stack_merge_authorization --> stack_merge_gate: authorization received; refresh facts
    merging --> reconciling
    reconciling --> done: remote confirms merge
    state blocked
    collecting_facts --> blocked
    assessing_risk --> blocked
    assessing_review_risk --> blocked
    planning --> blocked
    validating_decomposition --> blocked
    authorizing_direct --> blocked
    authorizing_children --> blocked
    awaiting_direct_execution_authorization --> blocked
    awaiting_child_execution_authorization --> blocked
    materializing_children --> blocked
    executing_children --> blocked
    preparing_independent_set_review --> blocked
    reviewing_independent_set --> blocked
    releasing_independent_children --> blocked
    awaiting_independent_landing --> blocked
    awaiting_independent_re_reviews --> blocked
    reconciling_child_set --> blocked
    preparing_stack_review --> blocked
    reviewing_stack --> blocked
    implementing --> blocked
    routing_implemented_change --> blocked
    publishing_direct_change --> blocked
    publishing_child_change --> blocked
    publishing_integrated_change --> blocked
    leaf_review_direct --> blocked
    leaf_review_child --> blocked
    awaiting_parent_handoff --> blocked
    integrating --> blocked
    integration_review --> blocked
    merge_gate --> blocked
    awaiting_remote_checks --> blocked
    awaiting_remote_facts --> blocked
    refreshing_direct_review_facts --> blocked
    refreshing_integration_review_facts --> blocked
    assessing_refreshed_integration_risk --> blocked
    validating_refreshed_integration --> blocked
    refreshing_independent_child_review_facts --> blocked
    requesting_independent_child_review --> blocked
    stack_merge_gate --> blocked
    awaiting_stack_merge_authorization --> blocked
    awaiting_stack_checks --> blocked
    awaiting_stack_facts --> blocked
    refreshing_stale_stack_facts --> blocked
    landing_stack_entry --> blocked
    reconciling_stack_entry --> blocked
    stack_rebase_gate --> blocked
    awaiting_stack_rebase_authorization --> blocked
    rebasing_stack --> blocked
    stack_republication_gate --> blocked
    awaiting_stack_republication_authorization --> blocked
    publishing_rebased_stack --> blocked
    refreshing_rebased_stack_facts --> blocked
    requesting_stack_child_reviews --> blocked
    awaiting_stack_child_reviews --> blocked
    merging --> blocked
    reconciling --> blocked
    blocked --> collecting_facts: authorized replan
    blocked --> cancelled: cancel
    done --> [*]
```

`failed` and `cancelled` are terminal classes. `blocked` is operator-owned and non-terminal. A
Workflow may define additional states, but every active state MUST have an activity, a child
barrier, a review barrier, an external wait, or an operator gate. A review barrier owns immutable,
distinct reviewer assignments and does not emit `approved` until its declared quorum is satisfied.

## 12. End-to-End Flow

### 12.1 Task-first low-risk flow

```mermaid
sequenceDiagram
    participant Source
    participant Harness
    participant Facts
    participant Agent
    participant Reviewer
    participant GitHub

    Source->>Harness: Submit Issue or task
    Harness->>Facts: Collect deterministic facts
    Facts-->>Harness: Evidence snapshots
    Harness->>Harness: Compute risk floor = low
    Harness->>Agent: Plan and implement in isolated workspace
    Agent-->>Harness: Change and validation evidence
    Harness->>GitHub: Idempotently publish or bind remote change
    GitHub-->>Harness: Durable remote change identity and current head
    Harness->>Reviewer: Fresh-context leaf review
    Reviewer-->>Harness: Head-bound approval receipt evidence
    Harness->>GitHub: Refresh PR facts and required checks
    GitHub-->>Harness: Pending, unavailable, failed, or current eligible head
    Harness->>Harness: External wait while facts are pending/unavailable
    Harness->>GitHub: Squash merge
    Harness->>GitHub: Re-fetch merged state
    GitHub-->>Harness: Merge confirmed
    Harness->>Harness: Mark done
```

### 12.2 Existing PR flow

An existing PR ingress creates a `WorkItem` bound to the observed repository, PR number, base ref,
and head SHA through the same `RemoteChangeBinding` used by issue-first publication. Fact
collection runs before any Agent. The Workflow may route directly to repair or Harness review, but
never directly to merge-gate evaluation. Any new push invalidates prior code-bound review and gate
evidence.
The registered fact collector is the sole author of these routes: task/issue ingress emits
`implementation_required`; existing PR ingress emits `repair_required` or `review_ready` from
trusted provider facts. A `review_ready` PR still passes through `assessing_review_risk`, so every
newly ingested PR has a current `SemanticRiskAssessment` and Harness `ReviewReceipt` before the
merge gate; provider review status cannot bypass either requirement. Missing, malformed, stale, or
abstaining semantic output routes to `blocked`, never to review or merge authorization. An Agent
cannot self-select a later state. If that review requests changes, the Work Item returns to
`collecting_facts`; the registered collector emits `repair_required`, and risk assessment,
planning, and execution authorization run before any repair mutation.

### 12.3 High-risk flow

High-risk work may collect facts, run semantic assessment, and produce a plan. Before a mutating
activity is dispatched, Harness requires an execution authorization receipt scoped to the exact
Work Item, Workflow hash, risk assessment, plan revision, and authorized writable scope. Runtime
workspace enforcement MUST reject an attempted write outside that scope before mutation and return
the Work Item to authorization rather than relying on a later publication gate. Merge remains
human-only even after implementation and review.

## 13. Risk and Authorization

### 13.1 Risk computation

Risk is an ordered set:

```text
low < medium < high
```

Risk is computed in two stages:

```text
project_assessment = max(workflow_deterministic_floor,
                         semantic_classifier_result)

effective_risk = max(server_universal_floor,
                     operator_non_lowerable_floor,
                     operator_escalation,
                     valid_human_override
                       ? human_override_level
                       : project_assessment)
```

A classifier may return `low`, `medium`, `high`, or `abstain`. `abstain`, missing output, malformed
output, unsupported execution enforcement, or stale inputs MUST NOT reduce risk. The Workflow
declares the fail-closed route, subject to server minimums.

A human risk override substitutes only for `project_assessment`; it never lowers the universal or
operator floor and never cancels an explicit operator escalation. It is valid only when bound to
the exact Workflow definition, input-fact revision, affected scope, plan revision when present,
code identity when code exists, issuer, reason, issue time, and expiry. Any bound change invalidates
the override before another mutating or merge action.

### 13.2 Risk floor inputs

The universal server floor SHOULD cover facts whose unsafe interpretation would compromise the
runtime itself. Project-specific path and domain rules belong in Workflow policy.

Candidate inputs include:

- protected paths;
- authentication, authorization, payments, secrets, or cryptography;
- schema/data migrations;
- dependency additions or security-sensitive upgrades;
- destructive commands or deletion scope;
- generated or vendored code;
- diff size, file count, and subsystem count;
- required test unavailability;
- unknown or incomplete remote facts;
- cross-repository impact; and
- manual override history.

### 13.3 Authorization receipts

```text
AuthorizationReceipt
  receipt_id
  authority_kind
  actor_id
  action
  work_item_id
  workflow_definition_hash
  plan_revision?
  code_identity?
  input_fact_revision
  scope
  effective_risk
  reason
  issued_at
  expires_at?
  revoked_at?
```

Receipts are action-specific. Plan approval is not merge approval. A receipt becomes invalid when
its bound definition, plan revision, code identity, risk, or expiry changes.

Low-risk automatic execution or merge still requires an `AuthorizationReceipt`. The trusted
`server_policy_engine` may issue it only in the same accepted transaction as an eligible gate result
bound to current fact Evidence, risk, action, definition, and code identity. Human-required routes
cannot receive a policy receipt. A gate result is never implicitly treated as authorization.
For a stack entry, the policy receipt additionally binds the integration-progress generation,
landing cursor, current remote binding, and `merge_current_stack_entry` action.

## 14. Layered Planning and Decomposition

### 14.1 Internal Agent plan

The Agent may freely refine a plan inside one activity or Work Item, provided it remains within the
delegated intent, permission scope, budget, and Workflow contract. Harness may persist the plan as
an artifact for observability, but does not schedule its checklist nodes.

### 14.2 Promotion criteria

A plan item SHOULD become a Child Work Item when one or more apply:

- it can execute independently;
- it needs a separate writable scope or workspace;
- it can run in parallel;
- it has a distinct acceptance contract;
- it needs a different capability or reviewer role;
- it has an explicit dependency relationship; or
- it should land as a separate or stacked PR.

### 14.3 `DecompositionProposal`

```text
DecompositionProposal
  proposal_id
  parent_work_item_id
  workflow_definition_hash
  based_on_evidence_ids
  revision
  rationale
  integration_strategy
  children[]
  dependencies[]
  acceptance_coverage
  predicted_write_scopes
  budget_allocation
```

Each proposed child includes intent, acceptance criteria, expected evidence, write/forbidden scope,
dependencies, risk hints, and budget.

### 14.4 Validation rules

Harness MUST reject a proposal when:

- it exceeds Workflow or server depth/child limits;
- the graph is cyclic;
- a dependency refers to an unknown child;
- required parent acceptance criteria are uncovered;
- sibling writable scopes overlap without serialization or an integration strategy that permits it;
- total child budget exceeds the parent's remaining budget;
- a child attempts to broaden repository, authority, or risk scope without approval;
- the proposed integration strategy is not allowed;
- the proposal is based on stale code or fact identity; or
- it duplicates active or completed work without an explicit replacement relation.

Materialization of the complete candidate batch and the parent barrier command MUST commit
atomically. Any illegal candidate aborts the transaction; a database test MUST prove that no child
is skipped or partially committed.

The `materializing_children` state is the sole caller of
`registered.decomposition_materialization.v1`. It consumes the validated proposal revision and its
bound execution authorization, then writes all child relations, child start commands, and the
parent barrier in one transaction. Agent output is only a proposal and cannot write these records.

### 14.5 Revision

An Agent cannot mutate a materialized graph. It may submit a new proposal revision. The validator
classifies changes as additive, replacement, cancellation, or dependency adjustment. Completed or
merged children are immutable. Revision may require renewed plan authorization when risk or scope
increases.

## 15. Integration Strategies

### 15.1 `independent_prs`

Use when each child is independently valid on the target base. Each child passes its own merge gate.
After leaf review each child waits at `parent_handoff`. The parent reviews the complete independent
set, releases eligible children to their own merge gates, then waits for typed remote-merge outcomes.
The parent does not create or merge an integration change and closes only after all required child
merges and set-level acceptance are reconciled.

### 15.2 `stacked_prs`

Use when child B depends on child A. The plan persists base/head relationships and landing order.
After leaf review, children wait at `parent_handoff`; the parent reviews the ordered binding set and
owns the ordered merge action. Approval of a lower stack head is invalidated when its effective diff
changes after rebasing. The parent merge subject is the ordered child binding set, not a synthetic
integration PR.

### 15.3 `integration_pr`

Use when intermediate changes must not land independently. Children produce reviewed change
artifacts or branches. A dedicated integration activity assembles them under one fenced workspace
and records exact child inputs. The integration PR receives a fresh parent review; child approvals
do not approve the assembled head automatically. Children stop at `parent_handoff` and cannot
publish or merge independently.

### 15.4 Strategy changes

Changing strategy after child execution begins requires a new decomposition revision and explicit
validation. Changing from separate landing to atomic integration, or the reverse, invalidates
affected integration and merge receipts.

## 16. Code Agent Runtime Contract

Workflow activity requirements are matched against trusted runtime profiles. The core must not
contain provider-specific product routing.

Minimum capability vocabulary SHOULD include:

- execution mode: `oneshot` or `per_turn`;
- structured output support;
- filesystem enforcement: `none`, `read_only`, or `isolated_write`;
- tool allowlist enforcement;
- network policy enforcement;
- approval handling;
- cancellation;
- timeout and stall detection;
- transcript capture;
- usage accounting;
- tool/event observability;
- model-selection evidence when a Workflow requires it; and
- fresh-context support.

Capabilities are not trusted merely because an Agent claims them. A configured adapter and runtime
host establish what can be enforced. `RuntimeCapabilitySnapshot` records the effective profile and
attempt-level enforcement evidence.

Agent mutation activities never receive provider credentials or provider-action authority. Their
contracts require `network: none` and `provider_actions: []`, while command access remains confined
to the authorized local workspace. Any attempted external write fails the activity. Publishing,
updating, or merging a remote object is possible only through a registered `provider_action` with
its own authority, idempotency, outbox, and reconciliation contracts.

Unknown events in an enforcement-sensitive mode MUST be handled according to a declared safe-event
policy. For a no-tool classifier, an unknown action-like event cannot be silently ignored and then
used as proof that no tools were used.

## 17. Result and Evidence Validation

Validation occurs in this order:

1. process and protocol completion;
2. runtime enforcement evidence collection across all attempts;
3. result extraction;
4. core envelope validation;
5. Workflow payload schema validation;
6. producer and role authorization;
7. activity-specific semantic validation;
8. freshness and code/source identity validation;
9. decision validation; and
10. reducer transition.

The validator MUST strip or reject server-reserved evidence kinds from Agent output. An activity
that permits exactly one business artifact MUST reject additional artifacts rather than allowing
them to satisfy unrelated terminal evidence requirements.

Formatting-only repair MAY be attempted within a bounded budget. Repair MUST NOT invent facts,
commands, tests, approvals, reviewers, code identity, or verdicts.

## 18. Review Architecture

### 18.1 Reviewer assignment

A review assignment is an immutable, server-issued supporting record. It records:

- assignment ID and schema version;
- author identity or identities;
- reviewer identity;
- reviewer role;
- reviewed code identity;
- reviewed scope;
- Workflow definition hash;
- issuer identity;
- required protocol; and
- independence mode;
- context-generation identity;
- effective read/write/tool permissions; and
- issue time, expiry, and revocation state.

The same effective identity cannot author and approve the same scope. Identity comparison uses
stable runtime/agent identity, not display name alone. Attempts and ReviewReceipt Evidence reference
the assignment ID; replay never reconstructs assignment authority from a display name or transcript.

Every code review assignment consumes one current `review_subject_snapshot`. The tagged snapshot
binds either one exact local change set/remote binding or a canonical complete child binding set,
including base/head/tree/diff identities and observation revisions. Publication results,
registered landing-path selection for an unpublished integration child, refreshed provider facts,
and registered parent-review materialization may produce it. The parent materializer consumes the
frozen child graph and every required terminal child outcome; for a stack, membership order and
expected-parent identities are part of the snapshot hash. A missing member, superseded identity,
or identity mismatch cannot create an assignment or receipt.

### 18.2 Review receipt

```text
ReviewReceipt
  review_id
  assignment_id
  reviewer_id
  reviewer_role
  independence_mode
  work_item_id
  scope
  review_subject_snapshot_id
  review_subject_hash
  base_sha?
  head_sha?
  diff_hash?
  workflow_definition_hash
  verdict
  findings
  protocol_status
  created_at
```

Allowed verdicts are Workflow-declared from a core protocol family such as `approved`,
`changes_requested`, `blocked`, or `abstained`. Missing or unparseable protocol output is
`protocol_failure`, never approval.

`review_subject_snapshot_id` is mandatory for both leaf and composition review. Leaf receipts may
also repeat their single base/head/diff fields for indexing. Composition receipts omit those
single-head fields and bind the snapshot's tagged aggregate `code_identity` through
`review_subject_hash`; reducers validate the immutable referenced Evidence rather than reconstruct
membership from receipt prose.

### 18.3 Child review

Every code-producing child requires an eligible current-head receipt before it can contribute a
successful child outcome.

For a stack rebase or stale stack receipt, fresh provider facts feed
`registered.stack_child_review_refresh.v1`. It atomically invalidates affected child outcomes,
materializes one child-owned, single-tagged `review_subject_snapshot` per affected Work Item from
its current remote binding, and enqueues `re_review_required` commands. Each child returns from
`parent_handoff` to `leaf_review_child`; only a new current leaf receipt regenerates its
`ChildOutcome`. The parent waits at `awaiting_stack_child_reviews` and cannot rematerialize the
stack review subject until every affected outcome is current.

For an independently released child whose merge gate detects stale review evidence,
`registered.independent_child_review_refresh.v1` performs the same child-owned snapshot and outcome
invalidation, notifies the parent with `child_review_stale`, and sends the child back to leaf
review. The parent waits at `awaiting_independent_re_reviews`, rematerializes the complete set only
after current child outcomes arrive, obtains a new parent review, and releases the set again.

### 18.4 Integration review

The parent reviewer checks:

- all acceptance criteria are covered;
- child outcomes correspond to the materialized decomposition revision;
- cross-child interactions and migrations are valid;
- the combined diff contains no unexplained scope;
- required validation ran against the integrated head; and
- no child failure or unresolved finding is hidden by aggregation.

For the standard high-risk profile, `integration_review` uses a review barrier with two distinct
immutable reviewer assignments. Each assignment produces its own head-bound receipt, and the
barrier emits `approved` only after both eligible receipts approve the same integrated head.
`review_integration` also requires a `validation_report` bound to the same current integrated head
as its `review_subject_snapshot`. When provider facts reveal a changed integration head,
`registered.integration_head_validation.v1` reruns the declared validation against that head before
the review barrier can be entered. The merge gate rejects an integration receipt whose validation
or review subject does not match the current remote binding.

### 18.5 Invalidation

A code-bound receipt is invalid when head SHA or effective diff changes. A Workflow may permit a
documented metadata-only exception, but the merge gate never infers that exception from prose.

The merge gate classifies stale review evidence from persisted subject/landing identity rather than
using one ambiguous retry edge. Direct subjects refresh provider facts and rerun semantic risk
assessment before review. Integration subjects refresh provider facts, rerun semantic risk and
registered validation against the refreshed head, and only then re-enter integration review. An
independent child refreshes its own leaf review and invalidates the parent set outcome. The stack
gate refreshes affected child leaf reviews before rematerializing the ordered aggregate. None of
these routes can reuse stale risk, validation, or review evidence or jump directly to merge.

## 19. CI and Merge Gate

The merge gate is deterministic and server-owned. Immediately before merge it MUST refresh and
validate at least:

- PR is open and not draft;
- expected repository and base branch match;
- current head SHA equals the authorized head;
- required checks exist and are successful;
- unresolved, non-outdated review threads are absent;
- required review receipts are current and valid;
- provider review requirements are satisfied;
- mergeability is acceptable;
- no child or integration barrier remains;
- Workflow definition and policy receipt remain valid;
- effective risk has not increased;
- the required merge authorization exists; and
- the merge method is allowed.

A human authorization receipt never routes directly to a provider merge action. It returns the
Work Item to the relevant merge gate, which refreshes every hard predicate above against current
provider state before creating the provider-action command.

Risk authorization is applied after all hard predicates:

| Risk | Execute code | Merge |
|---|---|---|
| Low | Automatic | Automatic when opted in and all gates pass |
| Medium | Automatic | Human authorization required |
| High | Human plan authorization required | Human-only; never policy-auto-approved |

Merge completion is not accepted from Agent output. Harness re-fetches provider state and records a
remote merge fact before terminalizing the parent.

Remote checks and facts have four distinct outcomes: `pending`, `unavailable`, `failed`, and
`eligible`. `pending` and retryable `unavailable` enter a bounded `external_wait` and re-evaluate the
gate when a new accepted fact arrives. `failed` follows the Workflow's repair/block route. Only
`eligible` may proceed to risk/authorization evaluation. None of these states is inferred as human
authorization or policy denial.

Review freshness is a separate gate outcome. `evaluate_merge_gate` emits exactly one of
`direct_review_stale`, `integration_review_stale`, or `independent_child_review_stale` from the
pinned landing relation. `evaluate_stack_entry_gate` emits `review_stale`. Each signal follows the
declared refresh and re-review path before risk or merge authorization is evaluated again.

## 20. Failure, Retry, and Recovery

### 20.1 Failure taxonomy

| Class | Example | Default handling |
|---|---|---|
| `configuration` | Invalid Workflow or missing schema | Block dispatch until corrected |
| `capability` | No runtime can enforce requirements | Block with explicit missing capability |
| `contract` | Malformed or unauthorized Agent result | Bounded repair, then block |
| `transient_runtime` | Timeout, temporary provider failure | Bounded retry with backoff |
| `external_dependency` | GitHub or CI unavailable | Wait/retry without losing current state |
| `policy_denied` | Risk or authorization forbids action | Operator gate or terminal denial |
| `stale_evidence` | Head changed after review | Invalidate evidence and re-enter required gate |
| `workspace_conflict` | Lease loss or overlapping writes | Stop, fence stale attempt, reschedule safely |
| `budget_exhausted` | Token, turn, repair, or child limit | Block with consumed-budget evidence |
| `semantic_abstention` | Classifier cannot decide | Workflow-declared escalation, never guessed success |
| `terminal_domain` | Impossible or unsafe requested outcome | Fail or cancel according to Workflow |

### 20.2 Retry rules

- Retry policy is declared by Workflow within server maximums.
- Retryability is classified before success-only attestations that could overwrite the original
  transient failure.
- Every retry has a stable reservation/idempotency identity.
- Provider actions reconcile an ambiguous remote result before retrying and never repeat an
  externally confirmed write.
- Retries preserve enforcement history relevant to the final acceptance decision.
- Repeated identical contract failures trigger suppression or operator attention.
- A terminal Work Item never reopens in place; retry creates a new run or an explicit recovery run.

### 20.3 Recovery

After restart:

1. replay the pinned Workflow and event log;
2. rebuild projections;
3. fence expired leases and orphaned attempts;
4. identify active states without an active command, job, wait, child barrier, or operator gate;
5. refresh external facts needed for reconciliation;
6. emit repair commands or block with evidence; and
7. never silently rewrite logical state to make projections look healthy.

An operator recovery receipt may retry only a declared failed gate or authorize `replan`. `replan`
returns to `collecting_facts`, where server-owned facts and risk are recomputed before any later
state. Recovery never jumps directly from `blocked` to implementation, review, or merge.

## 21. Workspace and Concurrency

- Every mutating activity executes in a leased isolated workspace unless a stricter Workflow policy
  forbids mutation entirely.
- Read-only semantic activities use an ephemeral empty workspace unless their declared input
  contract explicitly requires a repository snapshot.
- Presence of an optional JSON field is never sufficient to select a privileged workspace path;
  snapshots must be non-null and schema-valid.
- Output schema files, transcripts, and temporary runtime artifacts must be created in runtime-owned
  temporary storage, not an unleased source checkout.
- Sibling children with overlapping writable scope must be serialized or use an approved
  integration strategy.
- Lease generation fences stale workers from committing results.
- Workspace cleanup cannot erase evidence required for replay or review.

## 22. Persistence and Atomicity

The following must commit atomically for one accepted completion:

- runtime job terminal status;
- command status;
- activity result envelope;
- accepted evidence records;
- candidate and accepted decision records;
- workflow event;
- instance state/version;
- new command outbox rows;
- child materialization or barrier changes; and
- inline deterministic side effects owned by the runtime transaction.

External writes use an outbox/reconciliation model. A database commit never pretends an external
merge or tracker update succeeded.

Every persisted object includes schema/version information sufficient for replay. Unknown required
schema versions block replay explicitly.

`workflow_evidence` is the sole Evidence authority for every vNext definition. Review and
authorization receipts are typed Evidence payloads in that table. Dedicated receipt views or
indexes are derived from Evidence IDs and are rebuildable. Artifacts and evaluation tables never
become an alternate Evidence source.

Actor assignments and remote change bindings are immutable/versioned supporting records. An
attempt references its assignment and lease generation. Attempt observations and terminal folds
MUST verify the current lease generation; attempt numbers are allocated atomically per runtime job.

Entering an external wait atomically persists its wait identity, fact subject/cursor, refresh
contract, next delayed refresh command, bounded backoff, deadline, and budget reservation. A webhook
fact or refresh result may route the Workflow only after matching that identity. Each retry updates
the durable attempt count and schedules the next command. Remote failure, deadline expiry, and
budget exhaustion are distinct declared routes. Recovery treats a wait as healthy only when its
deadline, reservation, and scheduled refresh command are present.

Entering an operator gate atomically persists its requested action, prerequisite Evidence IDs,
accepted receipt kind, and compiled signal-to-route map. Direct execution, child execution,
direct/integration merge, stack-entry merge, stack rebase, and stack republication are distinct
actions. A validated receipt may emit only the matching declared signal; the API and reducer do not
infer a resume target from a state name.

Stack landing persists an `IntegrationProgress` cursor before its first gate. Each entry refreshes
facts and authorization against its current code identity, records the provider result, reconciles
that entry, and advances the cursor. Before rebasing remaining entries, an action-specific gate
authorizes the new expected-parent identity and writable scope; the rebase invalidates old
code-bound review Evidence. A second action-specific gate binds the resulting code identity before
an idempotent provider republication. Each successful update creates a superseding remote binding,
and a fact collector confirms the remote code identities before fresh review. Crash recovery
resumes from the persisted rebase/publish/landing cursors and remote facts; it never repeats an
externally confirmed write or treats a partial stack as complete.

`stack_rebase_context` normalizes the two legal rebase triggers: current merge-gate base drift and
post-landing advancement to remaining entries. It binds the integration-progress generation,
landing cursor, expected parent, remaining ordered bindings, current remote facts, and trigger. The
rebase authority gate and Agent activity consume that same context, so neither path depends on
Evidence that only the other path can produce.

## 23. Dual-Truth Reconciliation

Harness owns internal execution history; providers own current external state.

Reconciliation classifies divergence:

- `expected`: an authorized external write is not yet visible;
- `external_advance`: provider state moved forward independently;
- `external_regression`: PR closed, checks changed, approval dismissed, or head moved;
- `internal_gap`: event state has no command/job/wait explanation;
- `identity_mismatch`: provider object no longer matches bound repository/base/head;
- `unknown`: facts are incomplete or provider is unavailable.

Each classification emits evidence and a Workflow event. `unknown` is never treated as safe.

## 24. API and Operator Actions

Existing public identity rules remain:

- `submission_id` addresses submission reads;
- `workflow_id` addresses workflow actions;
- `request_id` is intake correlation; and
- workspace task IDs are not public handles.

The architecture requires actions equivalent to:

- submit Work Item;
- inspect trace, facts, evidence, decisions, children, reviews, and budgets;
- approve a high-risk plan revision;
- approve a medium/high-risk merge;
- raise risk;
- lower risk with a reasoned override;
- cancel;
- retry through a new run or recovery run;
- unblock to a declared recovery target; and
- recreate work under a new Workflow version by cancelling the old Work Item and submitting a new
  one with explicit replacement provenance.

All mutating operator actions create authorization/audit evidence before state changes.

## 25. Observability

Operators must be able to answer without reading database tables:

- What is this Work Item doing now?
- Which Workflow version controls it?
- Why was this Agent dispatched?
- Which facts and policy rules set its risk?
- What authority can it currently exercise?
- Which children exist and what blocks the parent?
- Who authored and reviewed each code identity?
- Which evidence is stale or invalid?
- Why is the system retrying, waiting, blocked, or refusing merge?
- Did the provider confirm the final merge?

Required metrics include:

- Work Items by lifecycle class and logical state;
- queue age and active lease count;
- Agent dispatches and skips by reason/fact hash;
- attempts, retries, repairs, and suppressions by activity;
- risk-level counts and human approval latency;
- decomposition depth, child count, and graph rejection reasons;
- review protocol failures, degraded independence, and stale receipt invalidations;
- merge gate rejection reasons;
- reconciliation divergence classes; and
- token/time budget consumption per Work Item and child graph.

## 26. Security and Threat Model

| Threat | Consequence | Required mitigation |
|---|---|---|
| Agent forges CI/review evidence | Unsafe merge | Producer classes are runtime-assigned; remote facts are server-collected |
| Author reviews own work | False independence | Stable identity separation and role validation |
| Head changes after approval | Stale review merged | Head/diff-bound receipts invalidated on change |
| Classifier uses forbidden tools | Contaminated semantic decision | Attempt-wide event enforcement and fail-closed unknown events |
| Agent injects extra artifact kinds | Bypassed evidence gate | Activity allowlist and reserved-kind rejection |
| Workflow requests unsupported isolation | Unenforced policy | Capability matching must prove effective enforcement |
| Child graph recursively explodes | Cost and operational exhaustion | Depth, count, budget, and revision limits |
| Concurrent children overwrite files | Lost or mixed changes | Writable-scope validation, leases, serialization, integration strategy |
| External state differs after DB commit | Incorrect terminal state | Outbox plus provider reconciliation |
| Human override remains valid forever | Stale authority | Scope, hash, risk, action, and mandatory expiry binding |
| Workflow update changes active rules | Non-deterministic replay | Immutable per-run definition pinning |

## 27. Success Metrics

| Metric | Current | Target | Measurement |
|---|---|---|---|
| Silent user-visible degradation | Partially guarded | 0 accepted cases | Contract and incident audit |
| Agent dispatch without persisted reason and fact identity | Multiple legacy paths | 0 | Runtime invariant test and metric |
| Merge using stale head-bound evidence | Merge step is partly head-pinned | 0 | Merge-gate integration tests |
| Same-identity author/reviewer approval | Not universally prevented | 0 | Review assignment invariant |
| Active run silently reinterpreted under new Workflow | Definition pinning exists | 0 regressions | Replay/version tests |
| Ordinary job bypassing required workspace lease | Regression found in PR #2010 | 0 | Workspace selection regression suite |
| Covered unchanged remote work dispatching an Agent | Partially prevented | 0 | Poll/reconciliation tests |
| Low-risk eligible change requiring human merge | Not measured | <5% policy exceptions | Authorization metrics |
| Medium-risk merge without human receipt | No unified receipt contract | 0 | Merge gate audit |
| High-risk mutation without plan approval | No unified plan gate | 0 | Dispatch gate audit |
| Unbounded retries/reviews/decomposition | Per-loop limits vary | 0 | Budget exhaustion tests |
| Restart recovery with duplicate active command/job | Partial recovery coverage | 0 | Database-backed crash/replay tests |
| Parent marked done with incomplete required child | No general child contract | 0 | Child barrier integration tests |
| Pre-vNext runtime rows imported into vNext | Current runtime data exists | 0 | Destructive-cutover acceptance test |
| Legacy reader/dual-write/compatibility runtime branches | Multiple current paths | 0 | Static search plus cutover integration tests |
| Binary/database epoch mismatch serves traffic | No vNext epoch contract | 0 | Startup matrix test |
| Pre-cutover provider object automatically becomes a vNext Work Item | Legacy intake rediscovers open objects | 0 | Fence conformance and ingestion tests |
| Old database credential/session writes after cutover begins | No enforced deployment epoch | 0 | Role revocation and held-session integration test |

## 28. Testing Strategy

### 28.1 Definition conformance

- the normative reference Workflow is extracted from this RFC (or generated from one canonical
  fixture) and passes the production parser, schema validator, linker, and compiler;
- every `operator_gate` declares exactly one requested action, accepted receipt kind, and a finite
  signal-to-route map;
- no undeclared field such as a prose-only dynamic action source is accepted;
- unknown state, activity, schema, capability, route, or producer fails validation;
- active state without progress mechanism fails;
- unsafe limits above server caps fail;
- content hash changes for semantic policy changes;
- invalid reload leaves existing pinned runs unchanged and reports an error.

### 28.2 Property and invariant tests

- effective risk never drops below the floor without a valid override;
- graph validation rejects cycles and uncovered acceptance criteria;
- lifecycle terminal states never dispatch ordinary work;
- an activity cannot consume evidence from an unauthorized producer class;
- author/reviewer identity overlap cannot produce approval;
- stale head-bound receipts cannot satisfy merge;
- command/job dedupe holds under concurrent dispatch.

### 28.3 Runtime and contract tests

- capability mismatch fails before process launch;
- structured-output retry retains first-attempt tool-use evidence;
- unknown enforcement-sensitive events fail closed;
- extra Agent artifacts are rejected when not allowed;
- transient failures remain retryable when success-only identity evidence is absent;
- invalid input returns a client error, not an internal server error;
- optional null snapshots do not select specialized execution paths;
- referenced-activity collection ignores unrelated global activity policy;
- route tokens are canonicalized before validation and comparison;
- semantic `blocked` status cannot bypass required decision/evidence validation; and
- declarative intake constructs required semantic input or returns a typed client error.

### 28.4 Parent/child tests

- valid proposal materializes children and parent barrier atomically;
- failed materialization creates no partial graph;
- sibling overlaps require serialization or permitted integration;
- child outcomes are typed and replayable;
- parent cannot complete with missing required children;
- a graph revision cannot mutate completed children;
- each integration strategy produces the expected review and merge gates.

### 28.5 Review and merge tests

- malformed reviewer output is protocol failure, never approval;
- same identity cannot fill author and reviewer roles;
- push after review invalidates the receipt;
- child approval does not satisfy parent integration review;
- unresolved current review thread blocks merge;
- missing required check blocks merge;
- medium/high merge lacks automatic authorization;
- provider confirmation is required before terminal `done`.

### 28.6 Crash and reconciliation tests

Inject crashes after each atomic persistence boundary and verify replay converges without duplicate
commands, children, or merge attempts. Simulate remote head changes, PR closure, CI regression,
review dismissal, provider timeout, and externally completed merge.

Breaking-cutover fixtures MUST prove that an old schema epoch cannot start under the vNext binary;
cutover requires explicit destructive confirmation bound to its manifest, source fingerprint,
locked counts, provider-fence digest, and epochs; old runtime rows are not imported; a full old
server and a worker holding an old database connection lose access when their exclusive role is
revoked and their sessions are terminated; a shared role refuses cutover; unknown catalog objects
refuse cutover; shared task rows outside the exact workflow predicate survive; the vNext listener
does not bind when runtime storage fails; a fresh vNext instance restarts from its compiled bundle;
and a stale worker cannot complete after lease reassignment. Provider fixtures MUST also prove that
final fence capture is refused until every active provider action is reconciled to a terminal
outcome and old provider credentials are revoked with zero writers verified; objects observed or
changed during drain fall at or before the final fence; every poll, webhook, and submission path
quarantines those pre-cutover objects; an unverifiable provider boundary disables automatic intake;
and explicit human-authorized adoption creates only new vNext identities.

### 28.7 Real dogfood profiles

At minimum:

1. low-risk documentation-only change through automatic merge;
2. medium-risk code change through human merge confirmation;
3. high-risk protected-path change blocked before mutation until plan approval;
4. large task decomposed into two non-overlapping children;
5. atomic integration PR with child and parent reviews;
6. existing PR ingestion and feedback repair;
7. service restart during child execution; and
8. Code-Agent substitution using the same Workflow contract.

## 29. Breaking Cutover and Relationship to Existing Designs

### 29.0 Document disposition

When this RFC is approved, it becomes the integrating architecture. Detailed documents remain
normative only where they do not conflict with it.

| Existing document | Disposition | Relationship |
|---|---|---|
| `workflow-declarative-definitions.md` | Extend | Preserve structural validation, blocked fallback, intake bindings, and definition pinning; add capability, evidence, risk, decomposition, review, authorization, and budget policy. |
| `prompt-workflow-contract-long-term-design.md` | Incorporate | Preserve compiled prompt packets, activity contracts, bounded repair, and typed results; place them under the Evidence and Activity contracts in this RFC. |
| `autonomous-github-intake-merge-spec.md` | Revise and incorporate | Preserve server-owned GitHub facts, fact-hash dispatch gates, quiescent ready state, and external merge confirmation; replace any Agent-authored fact or merge proof with provider-adapter evidence and tiered authorization. |
| `workflow-runtime-v2-state-machine-spec.md` | Extend with explicit supersession | Preserve workflow runtime ownership, atomic completion, outbox, recovery, and projections. Supersede the fixed list of permitted long-lived definitions where validated Child Work Items require additional instances. |
| `workflow-runtime-hardening-design.md` | Preserve | Its fail-closed output, reducer, retry, lease, and observability invariants become core conformance requirements. |
| `references/review-integrity.md` | Incorporate | Adopt fail-closed reviewer protocol, explicit independence identity, current-head receipts, and durable escalation evidence. |
| `runtime-submission-identity-contract.md` | Preserve | Keep `submission_id`, `workflow_id`, `request_id`, and workspace identity semantics. |
| `run-identity.md` | Extend | Use agent run identity as one input to stable author/reviewer identity; add remote-runtime identity binding without changing public submission identity. |

### 29.1 Preserve

- workflow runtime as the single orchestration truth;
- event-sourced reducers and command outbox direction;
- declarative definition validation and per-instance pinning;
- submission/workflow/run identity separation;
- deterministic server-owned GitHub facts and merge predicates;
- prompt packet compilation and activity result validation direction;
- workspace leases and runtime host model; and
- head-bound merge verification.

### 29.2 Extend

- declarative definitions with evidence schemas, capability requirements, risk, decomposition,
  review, integration, authorization, and budgets;
- activity contracts with producer roles and attempt-wide enforcement evidence;
- parent/child outcome handling with general Child Work Items;
- review receipts with stable author/reviewer identities and code identity;
- remote fact snapshots with a common trusted Evidence envelope; and
- runtime projections with child graphs, risk, authority, and reconciliation state.

### 29.3 Replace or retire

- prompt-only facts used where server-owned facts exist;
- runtime-kind-specific classifier routing in core workflow logic;
- arbitrary artifact kinds satisfying terminal evidence;
- mutable or implicit Workflow reinterpretation;
- agent-authored approval or merge proof;
- duplicated orchestration paths beside workflow runtime; and
- any vestigial configuration or data model without a live producer/consumer path.

### 29.4 PR #2010 disposition

PR #2010 should not be expanded by patching each review finding independently. After RFC approval,
its changes should be mapped to the target contracts:

- retain generic classifier policy concepts that fit Workflow-declared activity and evidence schemas;
- retain Code Agent structured-output integration that satisfies the generic capability contract;
- retain server-authored assessment and pinned policy ideas;
- rework workspace selection, attempt-wide enforcement, intake input construction, referenced
  activity selection, failure ordering, and artifact allowlisting;
- remove any provider- or classifier-specific core branching that belongs in Workflow policy; and
- use `docs/workflow-classification-minimal-path.md` for the independently reviewable current-runtime
  classification slice rather than treating this entire RFC as its prerequisite.

### 29.5 No-backward-compatibility boundary

The preserve/extend dispositions above refer to design ideas and implementation primitives, not
runtime data compatibility. vNext does not accept pre-vNext definitions, instances, events,
artifacts-as-evidence, serialized policies, or compatibility-only request payloads. No old run is
visible through the vNext API.

This RFC deliberately does not choose the physical deletion, retention, database fencing, or
provider-quarantine procedure. Those high-risk decisions are proposed separately in
`docs/workflow-vnext-cutover-rfc.md` and require real-database dry-run evidence plus explicit owner
and operational approval.

The cutover RFC requires a verified immutable audit archive before any source data is destroyed.
vNext never reads that archive; offline audit access is not runtime backward compatibility. Old
submission/workflow handles still have no vNext redirect or payload alias.

## 30. Implementation Plan

No phase may depend on declarations that are not wired into execution. Each phase requires focused
tests proving its end-to-end path.

No vNext phase below is authorized while this RFC is `Proposed`. The current-runtime generic
classification path is reviewed independently in `docs/workflow-classification-minimal-path.md`.
Approving that narrow path neither approves nor begins this vNext plan.

Even after owner approval of this umbrella RFC, each Phase 1-9 implementation scope requires a
separate owner approval before work begins. Phase 9 additionally requires the independent cutover
RFC and its operational/database gates to be approved; umbrella approval alone never authorizes a
production cutover.

### Phase 0 — Approve contracts

- Review this RFC and decision record.
- Resolve remaining schema questions.
- Mark conflicting older document sections as superseded or update them.
- Define conformance fixtures before implementation.

### Phase 1 — Compiled identity and attempt foundation

- Introduce the vNext database schema epoch and a schema-tagged compiled-bundle envelope before any
  vNext instance is accepted.
- Canonically pin every execution-relevant field supported at this phase, including referenced
  activity policies and dispatch constraints.
- Add immutable actor assignments and fenced activity attempts with input Evidence IDs,
  `lease_generation`, atomic attempt numbering, and capability snapshots.
- Make vNext runtime-store initialization and binary/database epoch mismatch fatal before listener
  bind. Keep the pre-vNext runtime as the only production runtime through Phases 1-8; dormant vNext
  components neither accept production traffic nor read old rows.
- Add startup and fresh-vNext replay tests. Do not implement or simulate the physical cutover in this
  phase; that work is gated by the separate cutover RFC.

### Phase 2 — Workflow compiler and Evidence foundation

- Add activity capability, evidence, risk, retry, and budget declarations to the compiled bundle.
- Compile payload JSON Schemas and pin the complete bundle for replay.
- Introduce the trusted Evidence envelope and producer classes with `workflow_evidence` as the sole
  vNext authority.
- Bind code/source identity and content hashes; represent authorization and review receipts as typed
  Evidence payloads, not independent authorities.
- Add immutable remote change bindings and adapt one existing remote fact collector without
  duplicating collection.
- Add strict docs, fresh-vNext replay, and example conformance tests.

### Phase 3 — Generic semantic activity

- Implement classifier as an ordinary Workflow activity profile.
- Enforce no-tool/read-only execution through trusted runtime evidence.
- Accumulate enforcement across correction attempts.
- Restrict output to declared evidence and decisions.

### Phase 4 — Risk and authorization gates

- Implement deterministic floor evaluation.
- Add semantic escalation/abstention.
- Add scoped human execution and merge receipts.
- Expose risk and authority in status APIs.

### Phase 5 — Layered decomposition

- Add proposal schema and validation.
- Materialize children atomically with parent barriers.
- Enforce graph, scope, depth, and budget bounds.
- Implement revision semantics.

### Phase 6 — Integration strategies

- Add independent, stacked, and integration PR contracts.
- Add workspace and dependency fencing.
- Persist typed child outcomes.

### Phase 7 — Review architecture

- Enforce assignment identities and ReviewReceipt Evidence introduced by the foundation phases.
- Enforce child and parent review separation.
- Fail closed on protocol or independence degradation.
- Invalidate stale receipts.

### Phase 8 — Merge and reconciliation closure

- Bind merge gate to fresh remote facts, risk, authority, reviews, and children.
- Confirm merge externally before terminal closure.
- Add crash/replay and remote divergence tests.

### Phase 9 — Activation and dogfood

- Require independent approval and completion evidence from `workflow-vnext-cutover-rfc.md` before
  any production activation.
- Route only new task-first and PR-first submissions through the standard profile; never import old
  runs.
- Run all dogfood profiles and publish evidence.
- Enable low-risk automatic merge only after the complete gate is proven.

## 31. Risks and Mitigations

| Risk | Severity | Likelihood | Mitigation |
|---|---|---|---|
| RFC becomes a second architecture beside existing specs | High | High | Explicit disposition table, implementation traceability, supersede conflicting sections |
| Workflow schema grows into an unmaintainable language | High | Medium | Thin core vocabulary, profile composition, server caps, conformance fixtures |
| Excessive flexibility weakens safety | Critical | Medium | Universal invariants cannot be lowered by Workflow |
| Core accumulates domain-specific hard-coding | High | Medium | Schema-driven payloads and generic activity contracts |
| Decomposition becomes a hidden general DAG engine | High | Medium | Promote only independently schedulable work; strict depth/count limits |
| Review identity appears independent but is not | Critical | Medium | Stable effective identities and explicit degraded mode that cannot approve |
| Remote fact collectors drift or duplicate | High | Medium | Reuse shared collectors and contract tests |
| Physical cutover is treated as implicitly approved by this RFC | Critical | Medium | Separate proposed cutover RFC with owner/operational gates and mandatory archive |
| Automatic merge exposes an incomplete gate | Critical | Low | Low-risk opt-in only after dogfood and merge invariant tests |
| Workflow authors cannot understand failures | Medium | Medium | Startup validation, typed error categories, trace/status surfaces |
| Agent cost explodes through children/review loops | High | Medium | Parent budget allocation, hard graph/retry/review limits |

## 32. Acceptance Criteria

This architecture is ready for implementation only when:

1. The RFC receives a fresh-context architecture review with no blocking finding.
2. The core-versus-Workflow ownership table is accepted.
3. Every domain object has identity, version, authorship, and replay semantics.
4. Workflow schema examples can express task-first, PR-first, direct, decomposed, and high-risk
   flows without provider-specific state-machine code.
5. Alternatives and rejected trade-offs remain documented.
6. Existing specs have an explicit preserve/extend/replace disposition.
7. PR #2010 has a file/commit-level salvage plan mapped to implementation phases.
8. Test fixtures are defined for risk, evidence, decomposition, review, merge, and replay.
9. Success metrics and operator-visible evidence are agreed.
10. No implementation phase requires a silent fallback or an unbounded loop.
11. The runtime boundary proves zero old-run import and zero compatibility runtime path; physical
    cutover approval remains explicitly outside this RFC.

The implementation is complete only when:

1. All required conformance, invariant, database-backed, crash/replay, and dogfood profiles pass.
2. No legacy parallel orchestration path remains for the adopted closed loop.
3. Low-risk automatic merge is demonstrably bound to current remote facts and independent review.
4. Medium/high merge requires a valid human receipt.
5. High-risk mutation requires valid plan authorization.
6. A large change can produce, run, review, integrate, and close Child Work Items without manual
   database or workspace repair.
7. The same Workflow can dispatch an alternative conforming Code Agent runtime without changing
   workflow state logic.

## 33. Resolved Questions from Phase 0

The Phase 0 map records the complete rationale. The normative outcomes are:

1. Payload schemas may be inline or under `.harness/schemas/`; resolved canonical content is hashed.
2. Universal and operator floors are non-lowerable; project path/domain rules belong to versioned
   Workflow policy unless an operator explicitly promotes them into its floor.
3. Independence uses immutable server assignments, distinct runs, fresh context generation, scoped
   permissions, and stable observable runtime identity; model diversity is optional policy.
4. A head change invalidates review. A new derived receipt requires proof of identical tree/diff and
   explicit Workflow permission.
5. Graph revisions invalidate affected evidence; medium risk requires human approval for expansion,
   while high risk requires it for every post-mutation revision.
6. Standard defaults are depth 2, 8 children per revision, 20 descendants, 2 primary attempts, 1
   correction per primary attempt, 2 repair cycles, 3 child and integration review cycles, and 3
   graph revisions, all capped by server ceilings.
7. Stack code identity binds repository, target, stack revision/position, expected parent, head,
   tree, and diff; rebasing creates a new identity.
8. This RFC owns architecture; future machine schemas own syntax. Older documents remain normative
   only for non-conflicting shipped behavior until explicitly superseded.

## 34. Related Documents

- `docs/workflow-first-autonomous-change-decisions.md`
- `docs/workflow-classification-minimal-path.md`
- `docs/workflow-vnext-cutover-rfc.md`
- `docs/workflow-declarative-definitions.md`
- `docs/prompt-workflow-contract-long-term-design.md`
- `docs/autonomous-github-intake-merge-spec.md`
- `docs/workflow-runtime-v2-state-machine-spec.md`
- `docs/workflow-runtime-hardening-design.md`
- `docs/references/review-integrity.md`
- `docs/runtime-submission-identity-contract.md`
- `docs/run-identity.md`
