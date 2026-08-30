# Workflow-First Autonomous Change — Phase 0 Current-State Map

Status: Proposed current-state map — owner approval pending

Date: 2026-08-28

Companion documents:

- `docs/workflow-first-autonomous-change-decisions.md`
- `docs/workflow-first-autonomous-change-rfc.md`
- `docs/workflow-classification-minimal-path.md`
- `docs/workflow-vnext-cutover-rfc.md`

PR baseline: PR #2010, commit `32dd03dfbf23ff97dff1e2dfadc18a4660a5547c`

## 1. Purpose

This document completes the repository-mapping part of Phase 0. It answers four questions before
implementation resumes:

1. Where do the six RFC domain objects already exist in Harness?
2. Which current contracts should be reused, extended, or replaced?
3. What exact boundary should the vNext Workflow compiler enforce?
4. Which changes in PR #2010 should be retained, rewritten, split, deferred, or removed?

This is not an implementation patch. No Rust behavior is authorized by this document alone. Agent
reviews have checked internal consistency and repository facts, but the architecture remains
`Proposed` until the owner approves it.

Current-runtime delivery has advanced independently of this vNext proposal: Slice A merged in PR
#2020 and Slice B merged in PR #2025. Slice C (assessment, routing, budgets, and replay) and Slice D
(production Codex-only dogfood) are not implemented. No vNext phase is authorized by those merges.

## 2. Executive Decision

Harness already has a viable orchestration kernel. The target architecture should extend that
kernel rather than create a second task system or replace the workflow runtime.

The durable pieces to preserve are:

- `WorkflowInstance` as the aggregate identity and current-state record;
- immutable definition identity and per-instance definition pinning;
- append-only workflow events and decisions;
- the command outbox and runtime-job lease model;
- atomic completion and terminal fencing;
- the existing `AgentBackend` abstraction;
- prompt-packet capture and structured activity results;
- server-owned validation and Evidence folding for runtime-captured provider transport receipts;
- workspace and runtime-host lease primitives; and
- current merge-verification semantics, with provider transport moved behind constrained
  AgentBackend prompts.

The missing architecture is not another scheduler. It is a typed contract layer above those
primitives:

```text
repository WORKFLOW.md
        |
        v
untrusted source AST
        |
        v
strict Workflow compiler ---- registered server contracts / runtime capabilities
        |
        v
immutable CompiledWorkflowBundle
        |
        +---- pinned to WorkflowInstance
        |
        v
generic activity dispatch -> attempt observations -> typed Evidence -> validated Decision
        |
        v
existing event / decision / command / runtime-job transaction path
```

PR #2010 must not be merged or cherry-picked as one unit. Its useful work is a prototype quarry:
model-observation events, strict structured output, pinned semantic policy, and server-authored
assessment are reusable ideas. Classifier-specific routing, public `classifier_input`, special
workspace branching, per-last-turn enforcement, and artifact-based evidence are not the target
contracts.

## 3. Evidence Method

This map uses three labels:

- **Fact**: directly visible in the current repository or PR #2010 snapshot.
- **Inference**: an architectural consequence of those facts.
- **Decision**: the proposed Phase 0 contract.

Repository references are paths and one-based line numbers from the current checkout unless they
are explicitly labeled `PR #2010`.

## 4. Current Architecture Findings

### F0-01 — The orchestration kernel is already durable and reusable

Severity: Architectural foundation

**Fact.** The runtime persists definitions, instances, events, decisions, commands, runtime jobs,
runtime events, and artifacts (`crates/harness-workflow/src/runtime/store_migrations.rs:6-95`).
`WorkflowInstance` already carries definition identity, logical state, subject, parent identity,
data, a mutation counter, lease, and timestamps
(`crates/harness-workflow/src/runtime/model.rs:76-98`). Commands have dedupe keys and a typed outbox
vocabulary (`crates/harness-workflow/src/runtime/model.rs:147-279`).

**Inference.** A parallel `work_items` orchestration store would duplicate authority and create a
reconciliation problem.

**Decision.** `WorkItem` remains a logical domain view over `WorkflowInstance` plus typed supporting
records. The existing workflow transaction path remains the only path that commits orchestration
state.

### F0-02 — The pinned definition is structurally sound but incomplete

Severity: Critical

**Baseline fact.** At PR #2019 creation, the compiler validated active states, targets,
reachability, terminal mapping, and
activity references (`crates/harness-workflow/src/runtime/declarative.rs:69-90`). The pin hash is
computed from `WorkflowDefinitionPolicy` only
(`crates/harness-workflow/src/runtime/declarative_pinning.rs:12-26`). Persisted metadata stores that
policy but not the complete activity execution contract
(`crates/harness-workflow/src/runtime/declarative_pinning.rs:29-47`). Historical hydration recreates
referenced activities as default policies
(`crates/harness-workflow/src/runtime/declarative_pinning.rs:58-68`).

**Current-head update.** PR #2020 added `declarative_workflow_identity.v2`, which hashes, persists,
and hydrates the resolved agent-contract map. The remaining vNext gap is the complete compiled
bundle outside that narrow contract family: evidence, risk, decomposition, review, authorization,
retry, budget, and recovery policy are not yet pinned together.

**Inference.** Prompt, validation, capability, evidence, retry, review, or authorization changes can
currently sit outside the canonical pinned bundle. Replay can therefore reconstruct the state graph
without reconstructing the exact execution policy.

**Decision.** The hash and persisted metadata must cover one canonical
`CompiledWorkflowBundle`: states, routes, control routes, terminal classes, activities, schemas,
intake bindings, risk, decomposition, integration, review, authorization, retry, budget, and
recovery policy.
Dispatch must consume only this pinned bundle, not reinterpret the current filesystem document.

### F0-03 — Activity policy is split between Workflow and hard-coded server matches

Severity: High

**Baseline fact.** At PR #2019 creation, `WorkflowActivityPolicy` declared only `prompt` and
`validation`. PR #2020 added the strict generic `agent_contract`, but the server still hard-codes accepted
signals, artifacts, no-op signals, and success requirements by workflow and activity name
(`crates/harness-server/src/workflow_runtime_worker/activity_contract.rs:13-173`). Runtime profile
selection is held in separate global/workflow/activity override maps
(`crates/harness-core/src/config/workflow.rs:220-274`).

**Inference.** Adding a semantic activity without Rust changes is not actually possible even though
the state graph is declarative. Policy ownership is split across the repository document, server
match arms, and global runtime configuration.

**Decision.** A declared `Activity` becomes the single abstract execution contract. Server-owned
work refers to a registered contract ID; agent work declares input/output schemas, allowed
decisions, produced evidence, required capabilities, permission policy, retry/repair policy, budget,
and prompt. Runtime selection remains operator configuration, but eligibility is checked against
the compiled activity requirements.

### F0-04 — Evidence has three overlapping representations and no universal envelope

Severity: Critical

**Fact.** `WorkflowEvidence` contains only kind, summary, and `ClaimProvenance`
(`crates/harness-workflow/src/runtime/model.rs:289-349`). Declarative completion converts every
non-`workflow_decision` activity artifact into self-declared workflow evidence by artifact type
(`crates/harness-workflow/src/runtime/declarative.rs:120-141`). Raw workflow artifacts are stored
separately (`crates/harness-workflow/src/runtime/store_migrations.rs:88-95`). A third
`workflow_run_evidence` table is shaped around project/stack/suite/baseline evaluation records
(`crates/harness-workflow/src/runtime/store_migrations.rs:649-708`).

**Inference.** Artifact presence, fact evidence, and evaluation evidence can be confused. The
current evidence record cannot bind producer class, Workflow hash, attempt, subject, code identity,
payload schema, freshness, or payload content hash in one validated envelope.

**Decision.** Add one append-only `workflow_evidence` source-of-truth table using the RFC Evidence
envelope. `ActivityArtifact` remains transport/output data and is never evidence merely because it
exists. `workflow_run_evidence` becomes an export/read model or a specialized consumer of accepted
Evidence; it does not remain a competing authorization source. Existing `ClaimProvenance` may be
embedded as proof metadata, but its ordinal trust level must not substitute for producer-class and
schema authorization.

### F0-05 — Runtime jobs do not model all activity attempts

Severity: Critical

**Fact.** `RuntimeJob` records one command execution and one final output
(`crates/harness-workflow/src/runtime/model.rs:501-595`). The server may run a structured-output
correction turn inside that job. PR #2010 checks tool use from `turn.items` only when building the
final result (`PR #2010: crates/harness-server/src/workflow_runtime_worker/executor/mod.rs:401-423`).

**Inference.** If the first turn used a forbidden tool and the correction turn did not, the final
assessment can lose the first observation. The same loss applies to approval, network, model, and
mutation observations.

**Decision.** Add `workflow_activity_attempts`, one row per actual Agent invocation, including
correction/repair turns. Every attempt produces an immutable `RuntimeCapabilitySnapshot` and binds
the same pinned input-envelope hash across primary/correction requests even though their prompt
packets differ. Activity completion folds all attempts with monotonic-denial semantics: any
forbidden observation makes the activity ineligible for a success decision.

### F0-06 — `WorkItem` exists only as loosely typed JSON and hard-coded projections

Severity: High

**Fact.** Declarative submission persists `submission_id`, prompt reference, source, external ID,
repository, and definition hash in `WorkflowInstance.data`
(`crates/harness-server/src/workflow_runtime_submission/declarative.rs:218-252`). Public status is
derived through `RuntimeWorkflowProjection`, but task phase/status still matches known state names
(`crates/harness-server/src/runtime_projection/mod.rs:202-253`).

**Inference.** Identity already has the correct aggregate, but risk, authority, integration,
budget, lifecycle class, base/head, and intent are neither one typed view nor fully independent of
built-in state names.

**Decision.** Introduce a typed `WorkItemSnapshot` read model. `lifecycle_class` comes only from the
pinned definition's terminal/progress metadata, never a state-name match. Stable, query-relevant
supporting objects use typed tables; non-concurrent descriptive fields may remain in classified
instance data during migration. Do not create a second mutable Work Item aggregate.

### F0-07 — Child workflows are durable primitives, not general Child Work Items

Severity: High

**Fact.** Parent identity is persisted on `WorkflowInstance`; child start validates a persisted
`StartChildWorkflow` command and parent/subject provenance
(`crates/harness-workflow/src/runtime/store/child_instance_start.rs:8-223`). The server dispatcher
supports only four named child definitions and branches by definition ID
(`crates/harness-server/src/workflow_runtime_worker/child_workflow.rs:25-54`). Parent completion is
propagated through definition-specific functions
(`crates/harness-workflow/src/runtime/worker.rs:377-434`).

**Inference.** The primitive safely starts known children, but it cannot represent a decomposition
revision, delegated intent, acceptance criteria, scope fences, dependencies, integration order, or a
typed terminal outcome.

**Decision.** Preserve child-start provenance and transaction logic as the low-level primitive. Add
`workflow_child_relations` and an atomic batch materialization transaction for a validated
`DecompositionProposal`. Any illegal candidate aborts the complete batch, and a database test must
prove that no child is skipped or partially committed. Replace definition-specific completion
propagation with a generic `ChildOutcome` envelope and Workflow-declared parent barrier.

### F0-08 — Risk, authorization, and review are not first-class closed-loop contracts

Severity: Critical

**Fact.** Current code has server-owned PR readiness facts and a server merge API, but no general
`ReviewReceipt` or `AuthorizationReceipt` type was found. The legacy issue store records a human
merge approval as a state event without a scoped, expiring receipt
(`crates/harness-workflow/src/issue_workflow_store/merge_approval.rs:16-39`). PR readiness checks
GitHub approval, CI, merge state, base ref, draft state, and review threads
(`crates/harness-server/src/github_pr_snapshot/mod.rs:468-499`).

**Inference.** Existing gates are useful provider facts, but they cannot implement the accepted
tiered execution/merge policy or prove author/reviewer separation across arbitrary Code Agents.

**Decision.** Add append-only authorization and review receipts, both bound to Work Item, Workflow
hash, scope, code identity, issuer/actor identity, time, and invalidation state. GitHub review status
is remote fact evidence; it is not a substitute for the Harness independent-review receipt.

### F0-09 — `AgentBackend` is the correct neutral boundary, but capabilities are too coarse

Severity: High

**Fact.** `AgentBackend` already unifies one-shot and per-turn implementations and exposes execute,
stream, interrupt, terminate, steer, and approval operations
(`crates/harness-core/src/agent.rs:38-127`). `AgentRequest` carries permissions, allowed tools,
model, reasoning effort, sandbox, approval policy, timeout, and capability token
(`crates/harness-core/src/agent.rs:129-181`). PR #2025 added fail-closed
`AgentContractCapabilities` for the pinned semantic-attempt path. The general `capabilities()`
surface remains a flat list and does not describe the full vNext enforcement profile.

**Inference.** No new `AgentRuntime` trait is required. What is missing is a trusted, structured
capability descriptor and execution observation contract.

**Decision.** Keep `AgentBackend`. Add a versioned `AgentCapabilityDescriptor` and per-attempt
`RuntimeCapabilitySnapshot`. Workflow logic must not branch on `RuntimeKind`; an adapter-specific
enforcer may branch internally and then report generic capability facts.

### F0-10 — Issue-first work has no typed, versioned remote change binding

Severity: Critical

**Fact.** Existing code can persist a `BindPr` command and provider identifiers in instance data, but
there is no typed, versioned binding contract shared by task-first publication and existing-PR
intake. A task/Issue may reach implementation before a PR exists, and the draft closed loop
previously moved from a local change directly to review and merge-fact refresh.

**Inference.** A merge gate cannot identify or reconcile its remote subject unless creation or
binding is idempotent and persisted. A PR number embedded in generic instance JSON is insufficient
for provider reconciliation and code-identity invalidation.

**Decision.** Add `workflow_remote_change_bindings` as an immutable/versioned supporting record and
a Workflow-declared `provider_action` for idempotent publish-or-bind. This is not a seventh domain
aggregate: the Work Item remains the lifecycle authority, and the binding records the external
object on which provider facts, review, CI, and merge operate. Existing-PR intake and publication
create the initial version. A registered read-only provider reconciliation creates a new version
that points to the prior binding whenever trusted base/head/code identity changes; it never mutates
the old identity in place. The Work Item stores the exact current binding ID; refresh locks and
compare-and-swaps that pointer in the same transaction as successor insertion.

## 5. Six Domain Objects: Brownfield Mapping

| RFC object | Current source of truth | Reuse | Required extension | Explicitly avoid |
|---|---|---|---|---|
| `WorkItem` | `WorkflowInstance` + runtime projection + prompt payload | Instance ID, state, subject, parent, definition pin, timestamps, event history | Typed `WorkItemSnapshot`; lifecycle class; risk/authority/budget/integration links; code identity | A second task/work-item state store |
| `WorkflowDefinition` | `WorkflowDefinitionPolicy`, registry, `workflow_definitions` | Structural compiler, reachability, terminal mapping, registry freeze, version pin | Full compiled bundle, closed control routes, schema registry, policy sections, complete hash | Hashing only the state graph or reading mutable policy at dispatch |
| `Activity` | `WorkflowActivityPolicy`, server `ActivityContract`, `RuntimeJob`, prompt packet | Runtime job/outbox, prompt/result capture, leases, structured result | Unified declaration; executor contract; attempts; capabilities; decisions; evidence allowlists | Classifier-specific activity type or workflow/activity match arms |
| `Evidence` | `WorkflowEvidence`, artifacts, remote facts, run evidence | Provenance proof types, remote collectors, content hashing, server-reserved concept | Universal envelope/table; producer classes; schemas; subject/code/source identity; freshness | Treating every artifact as evidence or ordinal trust as authority |
| `Decision` | `WorkflowDecision`, `WorkflowDecisionRecord`, `DecisionValidator` | Candidate/record split, rejection record, append-only persistence, current-state validation | Decision kind, definition hash, policy rule, input evidence IDs, authority receipt, validation result | Agent-selected authority or silent invalid-output fallback |
| `ChildWorkItem` | Parent ID, `StartChildWorkflow`, child-start transaction | Parent linkage, command provenance, dedupe, child instance lifecycle | Relation table, proposal revision, graph/scope/budget validation, parent barrier, typed outcomes | General live DAG for every Agent TODO or definition-specific child branches |

Supporting records do not expand the six orchestration objects. `ActorAssignment` establishes who
may act under which role and permissions; `RemoteChangeBinding` identifies the provider-owned
change; the separate cutover proposal uses `ProviderIntakeFence`; receipts are typed Evidence
payloads.

## 6. Exact Persistence Decisions

### 6.1 Existing storage roles retained as vNext authorities

The following storage roles remain authoritative and are extended or rebuilt as required. vNext
does not import their pre-vNext rows; physical audit retention and deletion remain separate cutover
decisions:

- `workflow_definitions`: immutable compiled bundles by `(id, version)`;
- `workflow_instances`: current aggregate snapshot;
- `workflow_events`: append-only internal facts;
- `workflow_decisions`: append-only accepted/rejected decisions;
- `workflow_commands`: transactional outbox;
- `runtime_jobs`: one dispatchable activity execution container;
- `runtime_events`: runtime-job observation stream;
- `workflow_artifacts`: non-authoritative output/transcript storage;
- `remote_fact_snapshots`: provider-specific latest-fact cache; and
- runtime usage tables: metering and budget consumption.

### 6.2 New typed records

The target requires these records. Names are normative for design but may be adjusted in a schema
review if they collide with existing schema conventions.

```text
workflow_activity_attempts
  id primary key
  workflow_id foreign key
  command_id foreign key
  runtime_job_id foreign key
  activity_id
  attempt_number
  attempt_kind              # primary | correction | repair | review
  agent_run_id
  actor_assignment_id
  input_evidence_ids jsonb
  input_envelope_hash
  lease_generation
  runtime_profile_snapshot jsonb
  capability_snapshot jsonb
  prompt_packet_hash
  status
  started_at
  completed_at nullable
  unique(runtime_job_id, attempt_number)

workflow_evidence
  id primary key
  workflow_id foreign key
  workflow_definition_hash
  evidence_kind
  envelope_schema
  payload_schema
  producer_id
  producer_class
  producer_role
  subject jsonb
  activity_attempt_id nullable foreign key
  code_identity jsonb nullable
  source_identity jsonb nullable
  observed_at
  expires_at nullable
  content_hash
  payload jsonb

workflow_child_relations
  parent_workflow_id foreign key
  child_workflow_id foreign key
  decomposition_revision
  delegated_intent jsonb
  acceptance_criteria jsonb
  writable_scope jsonb
  forbidden_scope jsonb
  integration_strategy
  landing_owner             # child | parent
  completion_milestone
  remote_binding_required
  integration_order nullable
  dependencies jsonb
  required_output_evidence jsonb
  status
  unique(parent_workflow_id, child_workflow_id)

workflow_actor_assignments
  id primary key
  schema_version
  workflow_id foreign key
  workflow_definition_hash
  assignment_role
  scope jsonb
  code_identity jsonb nullable
  author_identities jsonb
  actor_identity jsonb
  issuer_identity jsonb
  protocol jsonb
  context_generation_id
  permission_snapshot jsonb
  issued_at
  expires_at
  revoked_at nullable

workflow_remote_change_bindings
  id primary key
  schema_version
  workflow_id foreign key
  binding_version
  supersedes_binding_id nullable foreign key workflow_remote_change_bindings(id)
  provider
  repository_identity jsonb
  remote_object_identity jsonb
  base_ref
  head_ref
  code_identity jsonb
  publication_idempotency_key
  reconciliation_state
  source_identity jsonb
  created_at
  unique(workflow_id, binding_version)
  unique(supersedes_binding_id) where supersedes_binding_id is not null

provider_intake_fences
  id primary key
  schema_version
  provider
  repository_identity jsonb
  subject_type
  cutover_at
  maximum_monotonic_identity jsonb nullable
  snapshot_source_identity jsonb
  snapshot_hash
  automatic_intake_status      # enabled_after_fence | disabled_unverifiable
  created_at
  unique(provider, repository_identity, subject_type)

workflow_external_waits
  id primary key
  schema_version
  workflow_id foreign key
  workflow_definition_hash
  state_id
  wait_kind
  subject jsonb
  last_fact_identity jsonb nullable
  refresh_contract_id
  refresh_command_id nullable
  refresh_attempts
  next_refresh_at
  deadline_at
  budget_reservation_id
  status
  created_at
  completed_at nullable

workflow_operator_gates
  id primary key
  schema_version
  workflow_id foreign key
  workflow_definition_hash
  state_id
  gate_kind
  requested_action
  required_evidence_kind
  bound_evidence_ids jsonb
  allowed_signals jsonb
  status
  created_at
  completed_at nullable

workflow_control_continuations
  id primary key
  schema_version
  workflow_id foreign key
  workflow_definition_hash
  control_route_id
  source_state_id
  source_state_generation
  driver_kind
  driver_id
  driver_context jsonb       # mode-specific wait/gate/barrier/parent-handoff identity
  driver_lease_generation nullable
  driver_deadline_at nullable
  driver_budget_reservation_id nullable
  driver_dedupe_identity
  status                    # active | restored | invalidated | failed
  created_at
  completed_at nullable
  unique(workflow_id) where status = active

workflow_integration_progress
  id primary key
  schema_version
  workflow_id foreign key
  workflow_definition_hash
  generation bigint not null
  decomposition_revision
  integration_strategy
  ordered_binding_ids jsonb
  landing_cursor
  rebase_cursor
  publish_cursor
  landed_binding_ids jsonb
  pending_code_identities jsonb
  published_code_identities jsonb
  review_rounds
  stale_review_rounds_at_cursor
  expected_parent_code_identity jsonb nullable
  current_entry_code_identity jsonb nullable
  status
  created_at
  updated_at
  unique(workflow_id, decomposition_revision)
```

Every landing, rebase, or publish cursor transition compares the expected `generation` and advances
it atomically with the cursor and code-identity updates. Stack authorization receipts and
`stack_rebase_context` bind that exact generation; a stale concurrent update or replay cannot
advance the row. Entering stack review atomically increments `review_rounds`; a stale-fact re-review
also increments `stale_review_rounds_at_cursor`, and only a successful landing-cursor advance resets
that per-cursor count. Recovery never reconstructs either counter from review receipts.

`provider_intake_fences` is seeded from the verified immutable cutover manifest before the vNext
epoch is installed. The source fence records live outside the runtime schema being replaced, and
the vNext listener cannot start until the installed records match that manifest.

The first implementation may store dependency/evidence ID arrays as JSONB, but validation and
foreign ownership checks remain typed application contracts. A later normalized join table is
permitted only when query or referential-integrity requirements justify it.

`AuthorizationReceipt` and `ReviewReceipt` are versioned payload schemas in `workflow_evidence`.
There are no independently authoritative receipt tables. A specialized receipt view or index MAY
be built from Evidence IDs for query performance, but it is rebuildable and reducers never resolve
conflicts between two receipt copies.

Attempt numbers are allocated atomically per runtime job. Every attempt observation and terminal
fold checks `lease_generation` against the current job lease; a stale worker cannot claim an attempt
number, append authoritative enforcement observations, or complete the activity.

A review assignment requires `code_identity`. ReviewReceipt Evidence is accepted only when its
assignment, Workflow, Work Item, role, scope, reviewer identity, context generation, permissions,
and code identity exactly match the immutable assignment and the assignment is current. An
assignment schema version is interpreted by its versioned reader; unknown versions fail replay.

### 6.3 Objects that do not get independent mutable roots

- `WorkItemSnapshot` is a read model, not a table with its own lifecycle.
- `Budget` is a value object backed by policy plus runtime usage/reservation records.
- `RuntimeCapabilitySnapshot` is immutable attempt data.
- `ActorAssignment` and `RemoteChangeBinding` are immutable supporting records, not orchestration
  aggregates.
- `ProviderIntakeFence` is immutable cutover support state, not a Work Item or Workflow lifecycle.
- `ExternalWait` is a durable progress/fencing record backed by scheduled outbox commands, not a
  second workflow state machine.
- `OperatorGate` persists action-specific receipt requirements and allowed resume signals; the API
  cannot invent a transition from the current state name.
- `IntegrationProgress` is a fenced cursor/read-write support record for one validated strategy and
  decomposition revision, not an independently mutable graph.
- `ChildOutcome` is typed Evidence for a declared readiness milestone or terminal child relation
  state; only the terminal outcome releases the child budget reservation.
- `RemoteFactSnapshot` remains the provider cache and emits or links accepted Evidence.
- `DecompositionProposal` is candidate Evidence; only the validated materialization transaction
  changes the child graph.

### 6.4 No-runtime-compatibility boundary

vNext uses a new database schema epoch and `bundle:v2` definition hash namespace. It does not read,
drain, migrate, backfill, or reinterpret the current integer metadata schema v1. This is a runtime
contract only; it does not authorize physical deletion of historical data.

`workflow_evidence` is the sole Evidence authority from the first vNext instance. There is no legacy
reader, imported/shadow Evidence, dual write, compatibility alias, active-instance drain, or
in-place Workflow-version migration. Artifacts and `workflow_run_evidence` remain non-authoritative
outputs/read models.

The physical transition, mandatory audit archive, catalog dry-run, database-role fencing, provider
quarantine, and rollback procedure are owned by the separate proposed
`docs/workflow-vnext-cutover-rfc.md`. Approval of this map or the umbrella architecture never implies
approval of that cutover.

### 6.5 Candidate source inventory for cutover feasibility

This is the current repository-derived candidate inventory for the separate cutover RFC. It is not
an executable deletion authorization. A read-only dry-run must validate it against the real
deployment or a current production clone. If any source migration advances, the candidate manifest
version and this table must change together; unknown or missing owned objects fail the dry-run.

| Scope | Exact current source baseline | Count/action | Preconditions and exclusions |
|---|---|---|---|
| `${workflow_namespace}_runtime` | Migration ledger variant `schema_migrations` or `workflow_runtime_schema_migrations`, applied versions `1..32`; tables `workflow_definitions`, `workflow_instances`, `workflow_events`, `workflow_decisions`, `workflow_commands`, `runtime_jobs`, `runtime_events`, `workflow_artifacts`, `workflow_prompt_payloads`, `remote_fact_snapshots`, `workflow_repo_memory`, `runtime_usage_events`, `runtime_job_lease_renewal_receipts`, `workflow_artifact_dependencies`, `runtime_job_completions_dlq`, `workflow_run_evidence`, `runtime_job_lease_issuances`; functions `enforce_remote_lease_proof_writer()` and `record_runtime_job_lease_issuance()`; triggers `trg_enforce_remote_lease_proof_writer` and `trg_runtime_job_lease_issuance` on `runtime_jobs` | Count every table under lock, fingerprint the whole dedicated schema, then drop/recreate it for vNext | The `pg_catalog` digest must exactly cover relations, columns, constraints, indexes, functions, and triggers. No unlisted object may be dropped. |
| `${workflow_namespace}_issue` | Ledger variant `schema_migrations` or `issue_workflow_schema_migrations`, applied versions `1..6`; table `issue_workflows` and its indexes/constraints | Count and fingerprint, then drop the superseded dedicated schema | No row is imported. |
| `${workflow_namespace}_project` | Ledger `schema_migrations`, applied versions `1..4`; table `project_workflows` and its indexes/constraints | Count and fingerprint, then drop the superseded dedicated schema | No row is imported. |
| `runtime_state_store` | Shared ledger `schema_migrations`, applied versions `1..4`; tables `runtime_state` and `runtime_state_store_legacy_backfills`; rows selected by the exact configured `store_key = RuntimeStateStore::store_key_for_data_dir(<configured data dir>)` | Count, fingerprint, and archive all matching rows; delete only the matching `runtime_state` snapshot before activation; retain the shared schema, both tables, unrelated store keys, and the matching legacy-backfill markers as a fence against re-import | Rotate the runtime-host registration/heartbeat epoch and verify zero accepted old-epoch hosts before the reset. vNext starts with no restored hosts or project caches; an old host cannot re-register or claim work with pre-vNext authority. |
| `task_db.workspace_cleanup_targets` | Task migration ledger through version 29; rows whose fixed predicate is `store_key = <configured task-store identity> AND runtime_workflow_id IS NOT NULL` | Complete fenced workspace/process cleanup first; assert the locked count is zero; retain the shared table | Never cross a `store_key` boundary or delete a workspace bookkeeping row merely to make cutover pass. Active cleanup must finish or cutover refuses. |
| `task_db.workspace_leases` | Rows whose fixed predicate is `store_key = <configured task-store identity> AND runtime_workflow_id IS NOT NULL` | Refuse while any matching row is `leased` or has a live process; after normal fenced cleanup, delete only matching released rows | Preserve every row for another `store_key` and every in-scope row with `runtime_workflow_id IS NULL`; never drop the shared table. |

Configured schema identifiers are validated and quoted; they are not interpolated from unchecked
input. Dedicated table counts use the manifest's fixed identifiers. Shared-table mutations use fixed,
parameterized predicates. The source fingerprint includes configured schema names, the accepted
ledger-table variant and applied versions, and the canonical `pg_catalog` digest. The operator's
data-loss acknowledgement is valid for one fingerprint/count snapshot only.

The runtime-state reset is operational fencing, not Workflow-data migration. The archived host and
project-cache snapshot remains audit-only. Activation requires an empty current snapshot plus a new
runtime-host authority epoch; replay, startup restoration, and heartbeats carrying the old epoch are
rejected rather than adopted into vNext. An eligible pre-activation rollback never restores that
revoked authority: after restoring the old database and deployment, the secret manager issues a
distinct rollback database-writer credential, provider-writer credential set, and host-authority
epoch. The restored deployment proves that only those rollback identities can write, and every
restored host explicitly re-registers before the listener, intake, or provider dispatch can open.

## 7. vNext Workflow Compiler Contract

### 7.1 Source ownership

`WORKFLOW.md` remains the repository-owned entry point. It may reference repository schema files
under `.harness/schemas/`. Operator-supplied definitions use the same schema and compiler. Central
base plus repository override may continue for deployment defaults, but the final merged source is
compiled once and pinned.

### 7.2 Compilation stages

```text
1. Parse
   YAML front matter + referenced schema bytes -> source AST

2. Normalize
   resolve defaults, canonical names, schema references, ordered maps

3. Link
   states -> activities -> schemas -> routes/control routes -> registered server contracts

4. Validate
   reachability including control targets, progress, capabilities, evidence producers, decisions,
   risk monotonicity, graph bounds, review/authorization gates, server ceilings

5. Canonicalize
   deterministic JSON form containing every execution-relevant field

6. Hash and version
   content_hash = sha256(canonical compiled bundle)
   semantic_version remains author-facing; storage version is collision-safe

7. Persist and freeze
   write complete CompiledWorkflowBundle before accepting a new instance
```

Compilation is all-or-nothing. Unknown required capabilities, schemas, registered contracts,
producer classes, decision kinds, or routes fail startup/registration. Optional unknown fields are
not silently ignored; schema evolution requires an explicit schema version.

### 7.3 Activity declaration boundary

The vNext activity contract has this normalized shape:

```text
CompiledActivity
  id
  purpose
  executor                   # agent | registered_server | provider_action
  registered_contract_id?
  allowed_current_states
  input_schema_id
  output_schema_id
  required_capabilities
  permission_policy
  required_evidence_rules
  produced_evidence_rules
  allowed_decisions
  idempotency_contract?
  authority_contract?
  requested_action?          # typed authorization action ID
  reconciliation_contract?
  binding_transition_contract? # expected pointer, successor kind, pointer update, CAS mode
  provider_precondition_contract? # expected absence/version, cardinality, atomic stale refusal
  action_agent_contract?       # registry-resolved and pinned for provider_action only
  retry_policy
  repair_policy
  budget_policy
  role
  prompt_template
```

These optional contracts are linked and included in canonical bundle hashing. For every authority
evaluator and authorized mutation target, the compiler requires `requested_action` and proves that
an automatic target and any `await_human` operator gate, or a human-only gate and its authorized
target, consume the same typed action. For a binding transition, the compiler validates the expected
pointer, produced successor Evidence kind, pointer update, and `compare_and_swap` mode against the
registered server contract or the server-owned completion fold of a provider-action descriptor.
Unknown actions, unlinked targets, mismatched human gates, unsupported concurrency modes, or
registry mismatches fail definition compilation.

`executor: agent` never names Codex, Claude, OpenCode, or a model. Runtime profiles advertise
capabilities; the dispatcher chooses an eligible profile using operator configuration. Model choice
is a runtime-profile concern and is snapshotted, not part of workflow state logic.

`executor: registered_server` names a server contract such as fact collection or deterministic risk
floor evaluation. The compiler checks that the contract exists and that its declared input/output
schemas and producer classes match.

`executor: provider_action` is a server-orchestrated outbox contract whose external step executes
only through a constrained, action-specific AgentBackend prompt. The registry descriptor supplies
that prompt contract and the server-owned completion/reconciliation fold; the resolved descriptor is
pinned in the compiled bundle rather than repeated in each Workflow activity. Harness validates the
authority, Evidence, current binding, expected provider precondition (absence or exact version), and
idempotency key before dispatch,
but no Harness crate invokes `gh`, `git`, or a mutating provider SDK. The action turn receives an
allowlist containing exactly the typed `requested_action`; scoped credentials come from its runtime
profile, never from prompt or Evidence. Its response is a candidate until the server validates a
provider-authenticated webhook or runtime-captured provider transport receipt and reconciles remote
truth.

The compiler still requires explicit idempotency, authority, provider-precondition, and
reconciliation contracts. It links the provider precondition to accepted gate Evidence carrying
expected absence or an exact provider version and verifies that the registered AgentBackend path can
invoke a provider-native atomic conditional write for its single-subject or per-entry cardinality.
Prompted check-then-write, local pointer CAS, and reconciliation do not satisfy this contract.
Per-entry stack publication retains separate versions/idempotency keys and reconciles partial
success without replaying completed entries. Read-only registered contracts that collect provider
facts also use constrained AgentBackend prompts; registered server code owns validation and folding,
not provider transport.

Before provider I/O, the constrained tool channel must compare its operation, subject, typed action,
idempotency key, and expected absence/version with the pinned action request. Omission or mismatch
fails without network dispatch, and no unconditional primitive for the same mutation is exposed. A
path that can validate these values only after a write cannot register as a provider action.

The transport receipt is an immutable `workflow_evidence` row with the
`provider-transport-receipt.v1` payload schema, `runtime_enforcement` producer class, and the attempt
ID in its trusted envelope. Runtime enforcement appends it from the actual constrained provider tool
event before model summarization. The payload binds lease generation/tool-event ID, registered
contract, repository/subject, an `operation_id` required for every request, mutation-only
`requested_action`, idempotency key and expected absence-or-version, credential/profile identity,
request/raw-response digests, provider request ID/status when available, exit status, timestamp, and
immutable raw-response artifact. The provider-action or read-only provider-fact descriptor declares
that enforcement output. Unrestricted shell output or Agent prose is ineligible. Only the server
validator may derive `remote_provider` or `server_fact_collector` Evidence from that receipt; the
Agent cannot select or inherit either producer class.

### 7.4 Progress modes

The compiled core vocabulary is:

- `activity`: progresses through an activity completion;
- `external_wait`: progresses from accepted external fact events or its persisted scheduled refresh
  command;
- `operator_gate`: progresses only from a validated operator action/receipt;
- `parent_handoff`: child waits for a parent command;
- `child_barrier`: parent waits for typed child outcomes;
- `review_barrier`: waits for the declared quorum of distinct immutable reviewer assignments;
- `control_return`: restores the exact persisted source state and driver after a denied control
  request; and
- terminal mapping to `succeeded`, `failed`, or `cancelled`.

`child_barrier`, `review_barrier`, and `control_return` are the required additions to the current
progress-mode enum. None is a general DAG executor: graph shape lives in validated child relations,
review quorum lives in compiled review policy and immutable actor assignments, and a control return
can target only the source state captured by its named compiled control route.

Entering `review_barrier` atomically creates the declared number of distinct reviewer assignments
and their review commands for one code identity. Recovery treats the barrier as healthy only when
every outstanding assignment has exactly one active command or terminal receipt. A changed code
identity revokes the assignments and receipts; only a complete eligible quorum emits `approved`.

Entering `external_wait` atomically creates a `workflow_external_waits` record, reserves its retry
budget, and enqueues a delayed idempotent refresh command. Webhook facts and refresh results compare
against the wait's subject and last fact identity. Each unsuccessful refresh advances persisted
backoff and cumulative attempt count. A changed but still nonterminal fact updates that same wait
record without consuming a refresh attempt and never recreates its deadline or budget reservation.
Only eligible/failed terminal facts,
remote failure, the original deadline, or cumulative budget exhaustion follow declared routes.
Completion cancels or supersedes the pending refresh command. Recovery accepts an active wait as
healthy only when its original deadline, reservation, cumulative attempts, and next refresh command
are all present.

Waits entered after a human merge receipt are distinct compiled states from pre-authorization
waits. They persist the receipt-bearing continuation and return only to the matching deterministic
revalidation activity; a pending check or temporarily unavailable fact cannot route back to an
initial gate and request the same authorization again.

Cancellation is a compiled control route with its own `cancel_work_item` action and
`cancellation_receipt`, not an operator-recovery signal. An operator may request it from any
nonterminal state outside an existing cancellation flow, but the Work Item first enters its
cancellation gate; duplicates retain the current gate or barrier. The transition atomically stores
the source state/generation and exact command, job, wait, gate, barrier, or parent-handoff driver with
its mode-specific context, lease, deadline, budget, and dedupe identities, and suspends
reduction/new dispatch without cancelling that driver. A parent handoff binds its child relation,
decomposition revision, expected parent command/signals, and dedupe scope even when no command exists
yet. Denial uses `control_return` to restore or consume that exact continuation once; it cannot
recollect facts, reset budgets/deadlines, or create a duplicate attempt, and an unprovable restore
blocks. Authorization invalidates the saved continuation before the runtime fences the current Work
Item, interrupts local work, and reconciles its own in-flight provider action.

Recovery treats the cancellation gate and `control_return` as driven only when exactly one active
continuation matches Workflow, compiled control route, source generation, and driver identities.
Missing, duplicate, mismatched, or consumed rows fail closed before authorization or restoration.
Denial atomically commits `active -> restored`, source-state return, and exact driver
reattachment/buffered completion; authorization atomically commits `active -> invalidated` and the
cancellation-reconciliation route before fencing. Replay cannot perform both terminal transitions.
Only a safe admission may begin child fan-out. Confirmed external completion follows its truthful
direct, integration, or cancellation-aware stack reconciliation route. A landed stack entry is
recorded without resuming rebase or landing: a complete stack succeeds, while remaining entries
proceed to cancellation fan-out. Ambiguity or a committed nonterminal mutation remains blocked
rather than becoming cancelled.

Cancelling a decomposed Work Item is recursive: a registered server activity issues idempotent
commands and derived child-specific receipts for every active relation. Each child uses the same
admission and descendant barrier before acknowledging upstream. The parent becomes `cancelled` only
after every descendant has acknowledged and released its reservation, or immediately when the
locked child set has no active relation.

Entering `operator_gate` atomically creates a `workflow_operator_gates` record from the compiled
state declaration. It binds the requested action, prerequisite fact/plan/code Evidence, accepted
receipt kind, and finite signal-to-route map. A receipt can emit only the matching declared signal;
direct execution, direct repair, child materialization, child repair, integration, integration repair,
provider publication, direct/integration merge, and stack-entry merge are distinct
actions; stack rebase and stack republication are also distinct from each other and from merge.
Replay uses the gate record and receipt Evidence, never an API hard-coded resume target.

The integration strategy is frozen into child relations and `workflow_integration_progress` before
children dispatch. A registered server activity deterministically selects the post-implementation
route from that relation: direct work publishes then reviews; independent/stacked children publish,
review, and stop at `parent_handoff`; integration children review without publishing and stop at
`parent_handoff`. Independent children merge only after parent release. Stack landing advances one
persisted binding cursor at a time, refreshes the current entry gate, and after each landed entry
rebases the remainder through an Agent activity. Harness then dispatches an idempotent provider
publish action per entry/revision, persists superseding bindings, refreshes remote code identity,
and requires new review. The rebase, publish, and landing cursors fence partial-write recovery. An
integration parent alone publishes the assembled binding. No strategy profile rewrites compiled
routes at runtime.

### 7.5 Runtime dispatch contract

The dispatcher performs these checks in order:

1. Load instance and pinned compiled bundle.
2. Resolve the exact active state and activity.
3. Verify prerequisite Evidence, risk, authority, budget, and dependency/barrier state.
4. Select a runtime profile whose trusted descriptor satisfies every required capability.
5. Snapshot activity policy, input Evidence IDs, runtime profile, prompt packet, and code identity.
6. Create the runtime job and first attempt atomically with the command claim.
7. Execute and append attempt observations.
8. Validate output schema, producer permissions, and attempt-wide capability enforcement.
9. Materialize accepted Evidence.
10. Validate the candidate Decision and commit event, decision, state, and next commands atomically.

At no point does dispatch reread `WORKFLOW.md` to reinterpret an active instance.

## 8. Resolutions to the RFC Open Questions

These are proposed Phase 0 resolutions and should replace the open list after architecture review.

### Q1 — Payload schema location

**Decision.** Support both inline schemas and repository files under `.harness/schemas/`. The
compiler resolves both to canonical JSON Schema documents, rejects external/network references, and
includes every resolved schema byte representation in the compiled-bundle hash. Duplicate schema
IDs with different canonical content fail compilation.

### Q2 — Universal versus Workflow risk rules

**Decision.** Core owns monotonicity, missing/stale-evidence escalation, authority requirements, and
non-bypassable merge/execution safety. Path patterns and repository/domain rubrics live in a shipped
standard Workflow profile and repository overrides. Operator configuration may add a non-lowerable
protected-path floor. No Rust match statement contains product-specific repository paths.

The project assessment is `max(workflow_deterministic_floor, semantic_result)`. A valid human
override may substitute a lower project-assessment level, but effective risk remains the maximum of
that level, the server universal floor, operator non-lowerable floor, and explicit operator
escalation. The override binds definition hash, fact revision, scope, plan revision, code identity
when present, issuer, reason, issued time, and expiry; any bound change invalidates it.

Automatic low-risk execution and merge still require an `AuthorizationReceipt` Evidence row. The
trusted `server_policy_engine` issues that policy receipt only alongside an eligible deterministic
gate result bound to current facts, action, risk, definition, and code identity. Human-only routes
cannot accept a policy receipt, and `merge_gate_result` is never implicitly authorization.

### Q3 — Stable independent Agent identity

**Decision.** Persist an `AgentExecutionIdentity` containing server-issued assignment ID and role,
agent run ID, backend registration ID, runtime kind/profile, provider account or remote-host identity
when observable, model identity and source, and context-generation ID. Independent review requires a
different assignment and run, reviewer role issued by Harness, fresh context, no inherited author
thread, and no author write capability. The same model family may be reused; model diversity is a
Workflow option, not the universal definition of independence.

The assignment itself is an immutable `workflow_actor_assignments` record. Attempts and receipt
Evidence reference its ID; the assignment snapshots scope, author set, protocol, context generation,
permissions, issuer, expiry, and revocation so replay does not infer independence from transcripts.

### Q4 — Review preservation across head changes

**Decision.** No review receipt survives a head SHA change. A provider adapter may create a new
derived receipt without rerunning the Agent only when it proves the code tree and diff hash are
unchanged and the Workflow explicitly permits that derivation. The old receipt remains immutable
and invalidated; it is never edited in place.

### Q5 — Graph revision after execution

**Decision.** Every accepted graph revision invalidates affected child/integration evidence and
reruns deterministic risk and authority gates. At medium risk, human approval is required only when
the revision expands intent, writable scope, dependency privilege, integration authority, child
count/depth, or effective risk. At high risk, any post-mutation graph revision requires human
approval. Low-risk non-expanding revisions may proceed automatically within Workflow limits.

### Q6 — Default bounds

**Decision.** The shipped standard profile defaults are:

- maximum child depth: `2`;
- maximum children per revision: `8`;
- maximum total descendants: `20`;
- maximum primary attempts per activity: `2` unless a registered server activity is explicitly
  idempotent;
- maximum structured-output correction attempts: `1` per primary attempt;
- maximum repair cycles: `2` per activity state;
- maximum findings-driven child review cycles: `3` per child Work Item;
- maximum findings-driven integration review cycles: `3` per integration Work Item;
- maximum stack review rounds: `32` per decomposition revision: at most `8` initial or
  landing-cursor-advancing rounds and at most `3` stale-fact re-review rounds at each landing cursor;
  every round increments the persisted integration-progress counters before review dispatch; and
- maximum graph revisions: `3`.

Server ceilings are equal or stricter and cannot be raised by Workflow. Exhaustion becomes an
explicit blocked/failed decision according to the declared route; it never becomes an unbounded
loop.

### Q7 — Stacked PR code identity

**Decision.** `CodeIdentity` for a stack entry contains repository, target branch, stack revision,
position, expected parent head SHA, entry head SHA, tree SHA, and diff hash against the expected
parent. Rebase creates a new code identity and invalidates head-bound evidence. When patch identity
is provably unchanged, a Workflow may request a derived review as described in Q4.

### Q8 — Normative schema reference

**Decision.** The architecture RFC is normative for ownership and invariants. Versioned machine
schemas under `.harness/schemas/` or a crate-embedded schema directory become normative for
syntax. `docs/workflow-declarative-definitions.md` remains the user guide and must be updated from
those schemas; it is not an independent source of schema truth.

## 9. PR #2010 Commit-Level Disposition

| Commit | Disposition | Reason |
|---|---|---|
| `3ca7df2c feat(runtime): add declarative model classifiers` | Do not merge/cherry-pick; mine by contract | It combines useful observations with classifier-specific schema, public input plumbing, special workspace behavior, incomplete attempt enforcement, and cross-layer refactors. |
| `32dd03df test(runtime): align Codex isolation assertions` | Re-derive in the capability-descriptor slice | The assertion expresses a useful desired capability, but depends on an over-broad runtime-kind label rather than an attempt-specific enforcement snapshot. |

The PR branch should remain available as reference until the generic activity/capability slices land.
It should then be closed as superseded, with links to the replacement PRs and this map.

## 10. PR #2010 File-Level Salvage Plan

Disposition vocabulary:

- **Retain**: preserve the behavior in a focused replacement change after review.
- **Rewrite**: preserve the intent but implement against the vNext generic contract.
- **Split**: separate an independently useful refactor/protocol change from classifier work.
- **Defer**: do not land until its owning architecture phase exists.
- **Remove**: do not carry the change forward.

| File | Disposition | Target phase | Required treatment |
|---|---|---:|---|
| `crates/harness-agents/src/anthropic_api.rs` | Retain | 2/3 | Emit generic backend model-observation evidence; do not call it classifier attestation. |
| `crates/harness-agents/src/claude_stream.rs` | Retain | 2/3 | Preserve propagation of model observations through the generic stream protocol. |
| `crates/harness-agents/src/claude_stream_json.rs` | Retain | 2/3 | Preserve parsing of provider-reported model identity with fail-closed malformed identity handling. |
| `crates/harness-agents/src/claude_stream_json_tests.rs` | Retain | 2/3 | Move into generic model-observation conformance fixtures. |
| `crates/harness-agents/src/codex.rs` | Rewrite | 2/3 | Keep explicit model argument and deny-tool launch ideas; express them as adapter capabilities/observations. Validate CLI flags and avoid claiming an outer sandbox that was disabled. |
| `crates/harness-agents/src/codex_exec_parser.rs` | Split | 1/3 | Keep broader tool-event visibility. Handle unknown security-relevant event kinds fail closed. Move the skill-budget warning reclassification to a separate diagnostic patch. |
| `crates/harness-agents/src/codex_exec_parser_tests.rs` | Split | 1/3 | Separate tool-observation tests from unrelated diagnostic-warning tests. |
| `crates/harness-agents/src/codex_tests.rs` | Rewrite | 2/3 | Test the effective generic capability snapshot and actual launch arguments, not `classifier_only`. |
| `crates/harness-core/src/agent.rs` | Retain | 1 | Add generic model identity observation to `AgentEvent`; document whether it is provider-reported or launch-derived. |
| `crates/harness-core/src/config/workflow.rs` | Rewrite | 2 | Replace optional `classifier` with the complete generic `Activity` declaration. |
| `crates/harness-core/src/config/workflow/classifier.rs` | Remove | 2 | Its verdict/instruction concepts move to output schema, allowed decisions, and activity prompt; no classifier-specific policy type remains in core. |
| `crates/harness-server/src/http/tests/runtime_worker_tests/mod.rs` | Defer | 2/3 | Re-derive expectations after generic capability resolution lands. |
| `crates/harness-server/src/intake/declarative_routing.rs` | Remove | 1/3 | Do not pass `classifier_input: None`; intake must collect/link typed facts or reject missing required inputs. |
| `crates/harness-server/src/services/execution/runtime_submissions.rs` | Rewrite | 1/3 | Accept subject/intent and authorized evidence inputs, not a classifier-specific opaque field. Map validation failures to typed 4xx responses. |
| `crates/harness-server/src/workflow_runtime_submission/declarative.rs` | Rewrite | 1/2 | Persist generic input Evidence IDs and subject identity. Require only activities reachable for the submitted route, not every global classifier policy. |
| `crates/harness-server/src/workflow_runtime_submission/declarative_project_tests.rs` | Remove | 1/2 | Drop classifier plumbing and replace with generic input-contract tests. |
| `crates/harness-server/src/workflow_runtime_submission/declarative_tests.rs` | Rewrite | 1/2 | Add generic submission, pin, evidence-input, replay, and typed 4xx failure fixtures. |
| `crates/harness-server/src/workflow_runtime_submission/mod.rs` | Remove | 1/2 | Do not export classifier-specific submission plumbing. |
| `crates/harness-server/src/workflow_runtime_submission/runtime_request.rs` | Remove | 1 | Remove public `classifier_input`; introduce versioned generic evidence/subject intake only after its trust boundary is defined. |
| `crates/harness-server/src/workflow_runtime_worker/executor/mod.rs` | Rewrite | 1/3 | Create explicit attempts and aggregate enforcement across all primary/correction turns before accepting output. |
| `crates/harness-server/src/workflow_runtime_worker/executor/permission_profile.rs` | Rewrite | 2/3 | Resolve generic required capabilities; remove `classifier_only`; emit trusted effective settings per attempt. |
| `crates/harness-server/src/workflow_runtime_worker/executor_contract.rs` | Split/Rewrite | 1/3 | Keep server-owned assessment, output validation, model/tool observations, and fail-closed behavior as generic services. Move unrelated server-activity method relocation to its own refactor or drop it. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/activity_policy.rs` | Rewrite | 2/3 | Compile a generic semantic activity packet from the pinned bundle and schemas; remove classifier packet/type branching. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/mod.rs` | Split | 2 | Prompt-builder extraction is mechanical and may land separately; it is not classifier functionality. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet_activity_policy_tests.rs` | Rewrite | 2/3 | Convert to compiled-activity and schema conformance fixtures. |
| `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` | Rewrite | 2/3 | Keep enforcement taxonomy, but compute it from actual resolved settings. `RuntimeKind` alone must not claim `CodexIsolatedReadOnly`. |
| `crates/harness-server/src/workflow_runtime_worker/turn_engine/helpers.rs` | Split | none | The file split is unrelated cleanup; omit from semantic PRs unless independently justified. |
| `crates/harness-server/src/workflow_runtime_worker/turn_engine/helpers/stream_completion.rs` | Split | none | Same as above; no architecture dependency. |
| `crates/harness-server/src/workflow_runtime_worker/turn_engine/turn_lifecycle.rs` | Retain/Rewrite | 1/3 | Preserve generic model observations and project-root override capability, but record them in attempt observations rather than classifier-owned shared buffers. |
| `crates/harness-server/src/workflow_runtime_worker/workspace.rs` | Remove/Rewrite | 2/3 | Remove the presence-based classifier bypass. Use generic workspace capability selection. PR code treats `"classifier": null` as present at line 247 and can bypass ordinary workspace admission. |
| `crates/harness-workflow/src/runtime/classifier.rs` | Remove/Rewrite | 1/3 | Move input/output schema validation, pinned policy digest, and server assessment into generic Evidence/Activity services; remove classifier constants and reducer API. |
| `crates/harness-workflow/src/runtime/completion_evidence.rs` | Rewrite | 1 | Replace the growing reserved-artifact array with registered producer/evidence rules. Server producer identity is assigned by the runtime, not protected only by stripping names. |
| `crates/harness-workflow/src/runtime/declarative.rs` | Rewrite | 2 | Compile generic activity contracts and only referenced activities. Keep strict route/reachability validation. |
| `crates/harness-workflow/src/runtime/declarative_pinning.rs` | Retain/Rewrite | 2 | Preserve the correction that execution policy affects identity, but hash and persist the entire compiled bundle, not only classifier policies. |
| `crates/harness-workflow/src/runtime/dispatcher.rs` | Rewrite | 2/3 | Snapshot the resolved generic activity, input Evidence IDs, profile, and capabilities. Remove `classifier_job_snapshot`. |
| `crates/harness-workflow/src/runtime/dispatcher/input.rs` | Split | none | Pure source-file extraction may land separately if it still improves maintainability; it is not required by the feature. |
| `crates/harness-workflow/src/runtime/dispatcher/tests.rs` | Split | none | Test relocation follows the optional dispatcher refactor, not the classifier replacement. |
| `crates/harness-workflow/src/runtime/mod.rs` | Remove/Rewrite | 1/3 | Export generic activity/evidence contracts; do not expose classifier-specific runtime primitives. |
| `crates/harness-workflow/src/runtime/reducer/declarative_completion.rs` | Rewrite | 1/3 | Preserve server-validated semantic routing, but route through allowed generic decisions/evidence. Failed/blocked results cannot bypass assessment via `on_blocked`. |
| `crates/harness-workflow/src/runtime/state_registry/versioning.rs` | Retain/Rewrite | 2 | Preserve full-policy collision checks using the canonical compiled-bundle hash. |
| `crates/harness-workflow/src/runtime/tests/declarative_interpreter.rs` | Rewrite | 2/3 | Replace classifier fixtures with generic semantic-decision, evidence, blocked, and correction-attempt fixtures. |
| `crates/harness-workflow/src/runtime/tests/declarative_validation.rs` | Rewrite | 2 | Test complete bundle linking, referenced activities, schemas, capabilities, allowed decisions, and safe failure routes. |
| `docs/workflow-declarative-definitions.md` | Defer/Rewrite | 2 | Keep current documentation true for the shipped schema. Add vNext only with executable conformance fixtures; point architecture ownership to the RFC. |

## 11. PR #2010 Blocking Defects Mapped to Contracts

| Observed defect | Contract that prevents recurrence |
|---|---|
| `"classifier": null` triggers workspace bypass | Typed activity execution mode; no presence-based semantic flags; profile eligibility before workspace preparation |
| Tool use checked only on the last correction turn | Explicit attempt rows and monotonic attempt-wide capability fold |
| Stateless correction lacks the original facts | Every correction reuses the pinned prompt, immutable input envelope, output schema, and input-envelope hash |
| Non-empty semantic facts have empty or partial provenance | One trusted-boundary coverage/digest validation using current non-legacy `WorkflowDataProvenance` before dispatch |
| Intake passes `classifier_input: None` | Compiled input contract plus typed intake Evidence construction |
| Every configured classifier activity is collected | Link only activities referenced by the selected compiled definition and route |
| Classifier input errors become internal errors | Typed boundary errors: malformed/missing caller input is 4xx; server/registry inconsistency is 5xx |
| Verdict whitespace mismatches declared routes | Canonical schema normalization before validation and routing |
| Model mismatch checked before failed turn status | Turn terminal status first; attestation only for otherwise successful candidate output |
| Extra artifacts can satisfy unrelated evidence | Produced-evidence allowlist and explicit Evidence materialization; artifacts are non-authoritative |
| `on_blocked` bypasses verdict assessment | Semantic activity status/decision contract validated before any route; blocked route cannot mint success facts |
| Unknown Codex events fail open | Adapter protocol schema and fail-closed handling for unknown security-relevant events |
| Runtime kind is treated as proof of isolation | Attempt-specific trusted capability snapshot and enforcement observation |

## 12. Conformance Fixtures Required Before Rust Implementation

Phase 0 is not complete until fixture formats are accepted. The fixture set must include:

### Definition compiler

- `workflow_vnext_minimal_direct.yaml`: one agent activity and terminal success;
- `workflow_vnext_closed_loop.yaml`: fact, risk, plan, implement, review, merge, reconcile;
- `workflow_vnext_decomposed.yaml`: child barrier and integration strategy;
- invalid unknown capability, schema, producer, decision, registered contract, and route fixtures;
- unreachable state and unbounded retry/graph fixtures; and
- canonical hash stability plus one-field semantic hash-change fixtures;
- automatic authority action missing/mismatched with its target activity or human gate rejects;
- binding-transition pointer/output/CAS mismatch with the registered contract rejects, and either
  typed field changes the canonical definition hash;
- unreferenced global activity policy does not change a definition hash or input contract; and
- route/verdict whitespace is normalized once before validation and comparison.

### Evidence and attempts

- agent-authored allowed evidence accepted;
- agent attempt cannot claim server/reviewer/human producer class;
- extra artifact cannot satisfy required evidence;
- code identity mismatch and stale evidence rejected;
- assignment schema, issuer, role, scope, actor, permission, context, and code identity mismatch each
  reject receipt Evidence;
- first attempt uses forbidden tool, correction attempt is clean, final result rejected;
- stateless correction receives the exact primary prompt, input envelope, output schema, and matching
  input-envelope hash plus only the prior output and structured validation error;
- non-empty facts with empty, partial, orphaned, legacy, ambiguous, or digest-mismatched provenance
  reject before dispatch; valid ancestor coverage, child override, and empty-facts/empty-sidecar pass;
- stale worker observation/completion after lease reassignment is rejected;
- model identity missing/mismatched on a required-model-observation activity; and
- unknown security-relevant adapter event rejects attestation.

### Runtime boundary regressions

- optional `null` snapshot does not select a privileged/specialized workspace path;
- missing or malformed caller semantic input returns 4xx, while registry inconsistency returns 5xx;
- intake either constructs the declared input Evidence or rejects before submission;
- transient failure remains retryable when success-only model identity is absent; and
- `blocked` activity status cannot route around required decision/evidence validation.

### Risk and authority

- semantic risk may raise but not lower deterministic floor and consumes the trusted current-code
  snapshot or an explicit no-code snapshot;
- semantic risk rejects inherited author/session context and runs from a fresh assignment bound only
  to its declared Evidence;
- missing fact forces abstention/escalation;
- review-ready existing PR cannot enter review or merge authorization without current semantic
  risk, and a head refresh invalidates and recomputes that risk before re-review;
- every newly published direct, child, or integrated head refreshes current code facts and semantic
  risk before review; an unpublished integration child does the same against its local change;
- every code-producing activity refreshes local code risk before a distinct publication authority
  gate can issue the exact `publish_change` or `publish_integrated_change` receipt;
- medium merge lacks receipt and waits;
- low automatic merge has a current server-policy receipt bound to the exact eligible gate inputs;
- low automatic stack-entry merge additionally binds integration generation, landing cursor,
  current binding/code identity, and `merge_current_stack_entry`;
- high mutation lacks plan receipt and waits;
- every direct, direct-repair, child-materialization, child-repair, integration, and
  integration-repair mutation
  rejects a missing, stale, or wrong-action execution receipt even when a gate result exists;
- every evaluator, operator gate, and authorized mutation target declares the same typed action;
- expired/revoked/wrong-scope receipt rejected;
- stack rebase follows execution risk tiers and binds its current context and output scope, while
  every stack republication requires a human receipt derived from the validated rewrite and bound
  to the exact pre/post identities; and
- human risk override is reasoned, scoped, expiring, and auditable.
- risk override invalidates on fact, definition, scope, plan, code, expiry, or revocation change.

### Decomposition and integration

- valid two-child non-overlapping proposal materializes atomically;
- cycle, depth, child-count, budget, acceptance-gap, and scope-overlap failures create no children;
- one failed child prevents parent barrier success;
- independent-set and stack aggregate repair cannot become direct parent implementation and must
  validate a new decomposition revision before materialization;
- typed child outcome drives the parent without definition-specific code;
- stacked rebase invalidates code-bound evidence; and
- independent children stop at parent handoff, receive one release, merge through their own gates,
  and do not create a parent integration binding;
- stacked children stop at parent handoff and the parent merges the ordered child binding set;
- partial stack landing followed by crash resumes from landing/rebase/publish cursors without
  repeating a confirmed remote write;
- rebased stack entries are idempotently republished, remote identities refreshed, and freshly
  reviewed before the next entry gate;
- independent or stacked child head change invalidates its risk, validation, review, and
  `ChildOutcome`; all three current-head gates rerun before a new outcome reaches the parent;
- independent-set and stack aggregate subjects derive identity-bound semantic risk before their
  quorum-bearing parent review barriers open;
- integration children cannot publish/merge and the parent alone publishes the integrated binding;
- an integration parent releases children only after its remote merge is reconciled and reaches
  `done` only after every pinned child records the terminal contribution acknowledgement;
- a stack stale/republication refresh dispatches child-owned re-review commands so every child CAS
  refreshes its own binding; no parent-level singular binding refresh can mix stack identities; and
- integration PR requires child and parent review receipts.

### Candidate cutover conformance for the separate RFC

These fixtures are inputs to `docs/workflow-vnext-cutover-rfc.md`, not approval to implement or run
a destructive transition:

- vNext binary rejects the pre-vNext database epoch before serving traffic;
- destructive cutover refuses to run without explicit operator confirmation;
- wrong source/target epoch, source fingerprint, manifest version, provider-fence digest, missing
  maintenance mode, or missing exclusive locks refuse mutation;
- the deletion manifest removes every superseded runtime surface and no unrelated table;
- cutover imports no old definitions, instances, events, jobs, artifacts, or Evidence;
- pre-cutover provider objects are never automatically adopted; explicit human-authorized adoption
  creates only new vNext identities;
- an unverifiable provider fence disables automatic intake for that binding;
- a held old-worker transaction is rolled back when its exclusive old role is revoked and its
  session terminated; reconnect with that credential fails;
- a shared database role refuses cutover;
- final provider-fence capture refuses while any old provider action lacks a reconciled terminal
  outcome;
- final provider-fence capture refuses until old provider credentials are revoked and the
  zero-provider-writer proof succeeds;
- runtime-state reset archives only the configured store key, preserves unrelated keys and the
  legacy-backfill fence, starts with empty host/cache state, and rejects old-epoch heartbeats;
- pre-activation rollback restores no revoked credential, mints distinct rollback database-writer,
  provider-writer, and host-authority identities bound only to the restored deployment, and rejects
  all pre-cutover and abandoned vNext identities before any listener, intake, provider dispatch, or
  restored host registration opens;
- provider subjects created or changed during drain are recorded at or before the final fence and
  remain quarantined from automatic vNext intake;
- vNext accepts no production submission and enables no provider credential before explicit
  activation atomically forfeits rollback; restore succeeds only before activation, while missing
  eligibility proof or any post-activation state refuses production restore and requires forward
  recovery;
- the vNext HTTP listener never binds when runtime storage or epoch verification fails;
- interrupted cutover either commits the complete new epoch or restores/fails closed;
- a fresh vNext Work Item restarts from its complete `bundle:v2`; and
- stale workers from before shutdown cannot write into the new epoch.

### Review and merge

- author/reviewer assignment or run collision rejects approval;
- inherited author context rejects fresh-context claim;
- head change invalidates receipt;
- a trusted external head change creates a superseding immutable remote binding version, and risk,
  validation, review, and merge consume only that current version;
- concurrent/replayed binding refreshes cannot fork successors and must compare-and-swap the Work
  Item's exact current binding pointer before downstream dispatch;
- leaf review rejects a missing, stale, wrong-subject, or non-passing validation report;
- high-risk independent-set, stack, and integration parent review requires two distinct eligible
  assignments for the same aggregate subject;
- every `changes_requested` route supplies the current findings-bearing receipt to its distinct
  repair planning or mutation activity;
- direct repair uses its own `repair_direct_change` action, authority gate, operator gate, and
  mutation activity rather than returning to initial direct implementation;
- repair of an already-bound PR permits only direct repair or abstention, so success cannot strand
  the original binding behind replacement child PRs;
- both existing-PR ingress outcomes run current-risk validation and Harness review before repair or
  merge; provider `repair_required` facts cannot enter generic planning or authorize mutation;
- issue-first publish retries resolve to one remote change binding and one provider object;
- Agent prose or a structured provider candidate without a matching runtime-captured transport
  receipt cannot mint provider/server-fact Evidence; request, subject, operation, mutation action,
  expected-absence-or-version,
  credential/profile, raw-response digest, or immutable-artifact mismatch rejects the receipt;
- a provider tool request that omits or changes the pinned operation, subject, mutation action,
  idempotency key, or expected absence/version is rejected before network I/O, and no unconditional
  mutation primitive is available to that action profile;
- direct and integrated publication declare a nullable-current binding CAS: null creates one initial
  binding, while a bound PR creates or reuses one successor for the same provider object; ambiguous
  completion and a lost CAS reconcile without repeating the provider write;
- CI pending enters external wait, unavailable retries within budget, failed follows repair/block,
  and only eligible reaches authorization;
- checks or facts that remain pending after human merge authorization preserve that receipt and
  resume the matching revalidation activity without a duplicate operator prompt;
- denied cancellation restores the exact source state and active/buffered progress driver once with
  its original lease, deadline, budget, and dedupe identities; restart during the gate or return
  cannot duplicate work or strand the continuation;
- denied cancellation from `parent_handoff` restores the same child relation, decomposition
  revision, expected parent command/signals, and dedupe scope without inventing a driver command;
- restart reconstructs exactly one external wait refresh command; webhook/refresh races dedupe by
  wait and fact identity; deadline and budget exhaustion take distinct routes;
- repeated queued/running check updates preserve one wait ID, original deadline, cumulative refresh
  count, and budget reservation without consuming refresh attempts until eligible, failed, expired,
  or exhausted;
- current remote facts, CI, threads, review, base, mergeability, risk, and authority are all required;
- direct and stack merge provider actions reject a missing or stale current binding, gate result, or
  action-specific authorization receipt before external dispatch;
- every publication or merge provider action rejects a stale expected absence/version before
  mutation; stack republication checks the condition separately and atomically for every entry;
- provider reports merged but follow-up snapshot is stale/missing, so Work Item does not close;
- crash after provider merge but before local commit reconciles to one terminal event; and
- an out-of-scope Agent write produces no accepted partial result, routes to `blocked`, and can resume
  only through operator replan, refreshed risk, and new action-specific authorization;
- cancelling a blocked decomposed parent fences and cancels every active child, waits for terminal
  child acknowledgements, and releases reservations before the parent becomes `cancelled`;
- cancellation requested from an integration authorization, repair, publication, or other
  nonterminal state follows the same action-specific gate and child barrier; an operator recovery
  receipt cannot authorize cancellation, repeated requests retain the current cancellation flow,
  and a depth-two child cannot acknowledge while a descendant remains active;
- cancellation first reconciles the current Work Item's own provider action: committed merge truth
  follows its truthful completion route; a partially landed stack records that entry and cancels
  the remainder without rebase or further landing, while unavailable, ambiguous, divergent, or
  nonterminal committed outcomes remain nonterminal and never fold to `cancelled`.

### Code-Agent substitution

The same semantic no-tool activity fixture must pass with every registered runtime that advertises
and proves the required capabilities. Codex is the first required dogfood runtime. A missing Claude
account cannot make the Workflow invalid and cannot block Codex-only operation.

## 13. Implementation Boundaries After Approval

If the umbrella vNext architecture is approved, its first implementation PR is limited to:

1. Add the vNext database schema epoch and `bundle:v2` definition identity.
2. Persist the minimal complete v2 bundle for currently supported execution-relevant policy.
3. Add immutable `workflow_actor_assignments`.
4. Add fenced `workflow_activity_attempts`, including input Evidence IDs, `lease_generation`, atomic
   attempt numbering, and capability snapshots.
5. Add the fatal vNext runtime-store/epoch startup path without activating it in production.
6. Keep the production pre-vNext runtime as the sole active runtime; the vNext
   code path accepts no production traffic and reads no old rows.
7. Add fresh-v2 identity, attempt, startup, and replay tests with no cutover behavior.

Implementation PRs may land dormant vNext components, but they must never deploy a dual-mode
runtime. Production activation remains blocked by the independent cutover RFC. The smaller current-
runtime classification path in `docs/workflow-classification-minimal-path.md` is evaluated
independently and does not imply approval of vNext.

The second PR extends the compiler and adds the single-authority Evidence foundation, typed receipt
schemas, remote change binding, and one adapted remote fact collector. The third implements a
generic semantic activity and uses Codex as its first conforming runtime. This order ensures that
Evidence never binds to an incomplete policy identity and prevents the classifier from inventing
another temporary evidence or policy channel.

No implementation PR may:

- add a classifier-specific field to `CreateTaskRequest`, `WorkflowActivityPolicy`, or runtime-job
  core logic;
- branch workflow state logic on `RuntimeKind`, provider, or model;
- derive Evidence from arbitrary artifacts;
- read mutable Workflow policy at dispatch for a pinned instance;
- treat process completion as activity success;
- claim a capability from configuration without effective runtime observation; or
- enable automatic merge before Phases 4, 7, and 8 are complete and dogfood evidence passes.

## 14. Phase 0 Exit Checklist

- [x] Map six RFC objects to current code and persistence.
- [x] Identify reusable primitives, duplication, and missing contracts.
- [x] Define the vNext compiler and runtime ownership boundary.
- [x] Resolve the eight RFC schema/identity/default questions as proposals.
- [x] Produce commit- and file-level PR #2010 salvage decisions.
- [x] Define conformance fixture inventory.
- [x] Receive initial fresh-context architecture and migration reviews.
- [x] Address their blocking architecture findings in the proposal.
- [x] Replace the RFC open-question section with proposed resolutions.
- [x] Receive fresh-context re-review with a machine-parseable advisory verdict.
- [x] Record machine-review results without treating them as owner approval.
- [x] Mark conflicting older document sections as superseded or update them.
- [ ] Receive owner approval for the umbrella architecture.
- [ ] Approve any vNext Phase 1 implementation scope.
- [ ] Review the physical cutover independently against real-database dry-run evidence.

Phase 0 has machine-reviewed proposed resolutions but no owner approval. It authorizes no vNext
implementation or production cutover. The minimal classification path is a separate proposal and
must preserve reusable generic contracts rather than merge PR #2010 wholesale.
