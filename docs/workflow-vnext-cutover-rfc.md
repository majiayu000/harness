# RFC: Workflow vNext Cutover and Audit Retention

Status: Proposed — owner and operational approval pending

Date: 2026-08-28

Related architecture: `docs/workflow-first-autonomous-change-rfc.md`

## 1. Decision Boundary

This RFC owns the physical transition from the pre-vNext Workflow runtime to vNext. It does not own
the vNext Workflow language, classifier behavior, Evidence semantics, or merge policy.

One constraint is already explicit: vNext will not read, import, reinterpret, backfill, or dual
write pre-vNext runtime records. That is a runtime compatibility boundary. It does not require the
destruction of historical audit data.

No destructive command, role revocation, schema replacement, or production cutover is authorized
while this RFC is `Proposed`.

## 2. Problem

Pre-vNext instances do not pin enough execution policy to be replayed safely under the vNext
contract. Treating them as vNext runs would create two meanings for policy, Evidence, review, and
authorization.

At the same time, old events, decisions, attempts, provider bindings, and Evidence may be required
for incident investigation, compliance, debugging, or operator accountability. Runtime
incompatibility therefore must not silently become audit destruction.

The production catalog may also contain far more unrelated schemas and objects than a clean test
database. The feasibility and cost of catalog inspection are unknown until measured against the
real deployment or a current production clone.

## 3. Proposed Outcome

Use an offline, one-way runtime cutover with a mandatory immutable audit archive:

1. atomically stop pre-vNext intake and prevent dispatch of new provider actions;
2. cancel active provider actions or drain them to reconciled terminal outcomes, recording every
   remote object created or changed during the drain;
3. revoke old provider credentials and verify zero pre-vNext provider writers;
4. revoke the pre-vNext runtime-host registration/heartbeat epoch and verify zero accepted old-epoch
   hosts;
5. only then capture the final provider intake fences in an immutable cutover manifest outside the
   runtime schema being replaced;
6. revoke old database-writer access, terminate its sessions, and verify zero remaining database
   writers;
7. create and verify a versioned audit archive from the fenced source;
8. verify an exact, versioned source manifest;
9. replace only the manifest-owned runtime objects and exact shared rows, including resetting the
   current store key's runtime-host/project-cache snapshot;
10. install the verified provider fences and then the vNext epoch; and
11. start vNext only after its critical storage/epoch handshake succeeds, with production intake
    and provider dispatch still closed; and
12. after the observation checks pass, require an explicit operator activation that atomically
    forfeits production rollback before opening intake or enabling vNext provider credentials.

vNext never reads the audit archive. Audit inspection uses an offline export reader or an isolated
restoration environment. This preserves the no-compatibility boundary without deleting the only
auditable record of earlier decisions.

```mermaid
flowchart LR
    Old[Pre-vNext runtime] --> Stop[Stop intake and new provider dispatch]
    Stop --> Drain[Cancel or reconcile active provider actions]
    Drain --> ProviderFence[Revoke credentials and verify zero provider writers]
    ProviderFence --> HostFence[Revoke runtime-host epoch and verify zero old hosts]
    HostFence --> Capture[Capture final provider fence manifest]
    Capture --> DbFence[Revoke old DB role and terminate sessions]
    DbFence --> Export[Create immutable audit archive]
    Export --> Verify[Verify digest and restore drill]
    Verify --> Manifest[Validate scoped source manifest]
    Manifest --> Replace[Transactional runtime replacement]
    Replace --> Epoch[Write vNext epoch last]
    Epoch --> Observe[Start vNext with production intake closed]
    Observe --> Activate[Explicitly forfeit rollback]
    Activate --> New[Open intake and provider dispatch]
    Export -. offline only .-> Audit[Audit reader / isolated restore]
    Audit -. no runtime import .-> New
```

## 4. Mandatory Audit Archive

The archive MUST be created after both provider and database writer fences and before destructive
data mutation. It MUST contain:

- manifest version and configured schema names;
- source schema epoch and applied migration-ledger variants;
- canonical schema/catalog fingerprint for every manifest-owned object;
- locked row counts and content digests for every exported table;
- complete pre-vNext definitions, instances, events, decisions, commands, jobs, runtime events,
  artifacts, prompt payloads, remote fact snapshots, run evidence, and usage/lease records;
- issue/project lifecycle rows owned by the superseded stores;
- exact exported shared rows selected by the Workflow ownership predicates;
- the configured runtime-state store key's host/project-cache snapshot and legacy-backfill markers;
- provider/repository/subject bindings needed to identify remote branches, issues, and pull
  requests;
- export tool version, start/end timestamps, database identity, and operator identity; and
- one root digest covering the manifest, metadata, and all content digests.

Before cutover, an isolated restore drill MUST prove that:

- the root digest verifies;
- every exported table count matches the locked source count;
- representative instance/event/decision histories can be queried;
- remote bindings can be enumerated without contacting or mutating the provider; and
- the archive can be retained under the configured retention and access policy.

A backup that has not passed this verification is not an archive and cannot satisfy the cutover
gate. “Optional backup” is explicitly rejected.

## 5. Scoped Catalog Dry-Run

Before implementing destructive execution, Harness MUST provide a read-only dry-run that runs
against a current production clone or, with operator approval, production itself.

The dry-run MUST:

- query only the configured runtime, issue, project, shared task, and shared runtime-state schemas;
- parameterize schema names and fixed shared-row predicates;
- inventory relations, columns, constraints, indexes, functions, triggers, and migration ledgers;
- report missing and unknown objects without mutating them;
- report the query plan and duration of every catalog query;
- count candidate rows without acquiring destructive locks;
- exclude unrelated per-workspace schemas from the fingerprint; and
- produce the exact digest that a later data-loss acknowledgement would bind.

Initial feasibility targets:

| Measurement | Target | Failure action |
|---|---:|---|
| Total catalog dry-run time | under 5 minutes | Redesign/scoped indexing review; no cutover implementation |
| Longest catalog query | under 30 seconds | Inspect plan and narrow query |
| Unknown objects in owned schemas | 0 | Update/re-review manifest or remove object explicitly |
| Unrelated schemas included | 0 | Treat as a correctness bug |
| Source rows modified | 0 | Treat as a critical safety bug |

These are proposal thresholds, not claims about the current production database. Actual results
must be attached before this RFC can be approved.

## 6. Source Manifest

The current candidate source inventory is maintained in
`docs/workflow-first-autonomous-change-phase-0-map.md` section 6.5. It currently describes runtime
migrations `1..32`, 17 runtime tables, two functions, two triggers, issue migrations `1..6`, project
migrations `1..4`, runtime-state migrations `1..4` with two shared tables, accepted ledger variants,
and exact shared task/runtime-state predicates.

Each shared task predicate binds both `store_key = <configured task-store identity>` and
`runtime_workflow_id IS NOT NULL`; neither column alone proves ownership.
The runtime-state predicate is separately fixed to the configured data directory's exact
`RuntimeStateStore::store_key_for_data_dir` value; it never selects another store key.

That inventory is a review input, not an executable authorization. Any intervening source migration
changes the manifest version and invalidates prior counts, fingerprints, approvals, and dry-run
evidence.

## 7. Writer and Provider Fencing

An advisory lock is not sufficient to fence already-connected writers. Before the archive snapshot,
a future approved cutover must use a Harness-exclusive old database role, revoke its
login/privileges, terminate its sessions, verify zero remaining sessions, and provision a distinct
vNext role. If the role is shared with a non-Harness client, cutover refuses.

The provider side is a separate write surface. Cutover first stops intake and new provider-action
dispatch. It then either cancels each active provider action or drains it to a reconciled terminal
outcome, recording every remote object created or changed. Only after revoking old provider
credentials and verifying zero pre-vNext provider writers may cutover capture the provider
boundary. A database writer fence does not prove this condition.

Runtime hosts are a third authority surface. Before archiving or resetting their persisted support
state, cutover revokes the pre-vNext registration/heartbeat epoch and verifies that no old-epoch host
is accepted. The archive includes the configured store key's snapshot and backfill markers. The
replacement step deletes only that store key's `runtime_state` snapshot, preserves both shared
tables, unrelated store keys, and the matching legacy-backfill fence, and starts vNext with empty
host/project-cache managers. Registration and heartbeat carrying the old epoch are rejected, so a
restored pre-vNext host cannot claim vNext work.

The final boundary includes every subject observed through the drain, so an object created by an
old runtime action cannot re-enter as vNext work. Subjects at or before that boundary remain fenced
from automatic vNext rediscovery; objects created by other actors during the maintenance window
also require explicit human-authorized adoption. Subjects created after the final boundary are
eligible for normal vNext intake because no pre-vNext provider writer remains. An unverifiable
boundary disables automatic intake for that binding. Adoption creates new vNext identities and
does not import old Harness runtime state.

The captured provider fence records and their digest are stored in the immutable cutover manifest,
outside the runtime schema that will be replaced. The replacement transaction installs those exact
verified records into vNext `provider_intake_fences` before writing the vNext epoch. The vNext
listener remains disabled until the installed fence digest matches the manifest, so schema
replacement cannot erase the boundary.

### 7.1 Rollback boundary

Full rollback is available only during a pre-activation observation window in which vNext accepts
no production submissions and has no enabled provider credentials. Before restoring the archived
database and old deployment, the operator must verify that intake and provider dispatch remained
closed, verify zero vNext provider writers, and prove from the server-owned outbox and
reconciliation records that no vNext provider action ever entered `dispatched`, `in_flight`,
`succeeded`, or `unknown`. Missing or incomplete proof refuses rollback.

Rollback never revives the runtime-host authority epoch or credentials revoked by the forward
fence. After the eligible database and deployment restore, the secret manager mints a distinct
rollback authority epoch, and every restored host must explicitly re-register under that epoch
before it can heartbeat or claim work. The listener and intake remain closed until the restored
runtime verifies the rollback epoch and rejects both the revoked pre-cutover epoch and the abandoned
vNext epoch.

The explicit activation transaction closes the rollback window before production intake opens or
provider credentials become usable. After that boundary, the archived database remains available
for audit and isolated restoration, but it MUST NOT be restored as the production runtime: accepted
submissions and provider writes cannot be preserved or undone by database rollback, and the old
runtime lacks their vNext history. An incident after activation quiesces vNext and uses forward
recovery or explicit operator reconciliation. This proposal does not introduce a replay bridge,
reverse provider fence, or dual-runtime compatibility path.

## 8. Alternatives

### A. Mandatory archive plus one-way runtime cutover — recommended proposal

Pros:

- preserves the no-runtime-compatibility constraint;
- retains auditable history;
- keeps vNext free of legacy readers; and
- supports full rollback before production activation and incident investigation afterward.

Cons:

- requires an export format, verification, retention policy, and restore drill;
- extends the maintenance window; and
- still requires high-risk database fencing.

### B. Keep legacy schemas online as read-only history

Pros:

- simple ad hoc SQL access; and
- no export format required initially.

Cons:

- old schema remains operationally coupled to production;
- permissions and accidental reads are harder to contain; and
- schema name and tooling collisions can recreate a compatibility surface.

Decision: not preferred. It may be reconsidered only if the archive restore/read path is
operationally inadequate.

### C. Dual-read or backfill old runs into vNext

Pros:

- one API could display all history.

Cons:

- reintroduces two runtime meanings;
- old execution policy cannot be reconstructed safely; and
- creates a permanent compatibility burden.

Decision: rejected by the owner’s no-backward-compatibility constraint.

### D. Delete source data with an optional backup

Pros:

- smallest cutover implementation.

Cons:

- unverifiable backups can lose the only audit history;
- violates the architecture’s auditability objective; and
- makes rollback depend on an untested artifact.

Decision: rejected.

## 9. Risks and Mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| Archive is incomplete or corrupt | Critical | Root digest, locked counts, representative queries, mandatory restore drill |
| Catalog inspection is infeasible on the real database | High | Read-only scoped benchmark before implementation; redesign on threshold failure |
| Old process writes during cutover | Critical | Exclusive role, credential revocation, session termination, zero-session verification |
| Old provider writer mutates remote state after archive | Critical | Drain provider outbox/attempts, revoke credentials, and verify zero provider writers before archive |
| Old runtime host is restored or resumes heartbeats | Critical | Archive then reset only the configured runtime-state snapshot, rotate the host authority epoch, reject old-epoch registration/heartbeat, and verify empty vNext host/cache state |
| Shared task rows are over-deleted | Critical | Fixed predicates, locked counts, cleanup preconditions, preservation tests |
| Old provider object re-enters as new work | High | Provider intake fences and fail-closed unverifiable bindings |
| Archive becomes an accidental runtime dependency | High | No vNext archive reader; offline tooling and isolated restore only |
| Rollback crosses incompatible credentials/data or remote side effects | Critical | Permit restore only before any vNext provider dispatch; otherwise quiesce and recover forward |

## 10. Approval Gates

This RFC cannot move to `Approved` until all of the following exist:

- owner approval of the retention and downtime policy;
- attached real-database dry-run results meeting the feasibility targets;
- an exact reviewed manifest version;
- a versioned archive format and successful restore drill;
- a tested old-role/session fencing procedure;
- a tested provider-writer drain, credential revocation, and zero-writer verification procedure;
- provider-fence conformance tests for poll, webhook, and direct submission;
- a rollback rehearsal that proves intake remains closed before activation, pre-activation restore,
  issuance of a distinct rollback host authority epoch, rejection of both superseded epochs, and
  post-activation refusal; and
- a runtime-host fence rehearsal proving scoped snapshot reset, unrelated-store preservation, and
  rejection of old-epoch heartbeats; and
- an independent operational/database review.

Approval of the umbrella Workflow architecture does not imply approval of this cutover RFC.

## 11. Open Questions

1. What retention period and access policy does the audit archive require?
2. Where is the immutable archive stored and who can decrypt/read it?
3. Is production dry-run acceptable, or must it use a current clone?
4. What maximum maintenance window is acceptable?
5. Which operator identities must approve the final digest?
6. What offline audit-reader format is sufficient for investigations?
