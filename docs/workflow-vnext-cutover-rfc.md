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

1. atomically stop pre-vNext intake and fence every new command/job claim or dispatch;
2. cancel or drain all active non-provider runtime jobs, terminate their agent processes, reconcile
   their completion candidates, and verify zero live workspace leases or owned processes;
3. cancel only provider actions proven never dispatched; reconcile every possibly dispatched action
   to remote terminal truth, recording every remote object created or changed during the drain;
4. revoke old provider credentials and verify zero pre-vNext provider writers;
5. revoke the pre-vNext runtime-host registration/heartbeat epoch and verify zero accepted old-epoch
   hosts;
6. only then capture the final provider intake fences in an immutable cutover manifest outside the
   runtime schema being replaced;
7. revoke old database-writer access, terminate its sessions, and verify zero remaining database
   writers;
8. create and verify a versioned audit archive from the fenced source;
9. verify an exact, versioned source manifest;
10. replace only the manifest-owned runtime objects and exact shared rows, including resetting the
   current store key's runtime-host/project-cache snapshot;
11. install the verified provider fences and then the vNext epoch; and
12. start vNext only after its critical storage/epoch handshake succeeds, with production intake
    and provider dispatch still closed; and
13. after the observation checks pass, require an explicit operator activation that atomically
    forfeits production rollback before opening intake or enabling vNext provider credentials.

vNext never reads the audit archive. Audit inspection uses an offline export reader or an isolated
restoration environment. This preserves the no-compatibility boundary without deleting the only
auditable record of earlier decisions.

```mermaid
flowchart LR
    Old[Pre-vNext runtime] --> Stop[Stop intake and all command/job claims]
    Stop --> RuntimeDrain[Drain jobs, processes, and workspace leases]
    RuntimeDrain --> Drain[Cancel undispatched actions; reconcile every possible dispatch]
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

Runtime execution is a separate authority surface from the provider outbox. Cutover first fences
all new command and job claims, including ordinary Agent activities, waits for claimed jobs to stop
or records and reconciles their terminal candidates, terminates their process groups, and verifies
that no runtime-owned workspace lease or process remains. A claimed ordinary job may not survive
until database-role revocation, and workspace cleanup is never used as a substitute for process
termination and reconciliation.

The provider side is another write surface. After the runtime drain, cutover may cancel an action
only when durable outbox/transport evidence proves it was never dispatched. Every dispatched,
possibly dispatched, running, returned, timed-out, or otherwise ambiguous action is reconciled to
authenticated remote terminal truth, recording every remote object created or changed. Only after
revoking old provider
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
also require explicit human-authorized adoption, including objects created after final fence capture
but before explicit vNext activation. Only subjects first created after the activation transaction
ends the maintenance window are eligible for normal vNext intake. An unverifiable boundary disables
automatic intake for that binding. Adoption creates new vNext identities and does not import old
Harness runtime state.

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

Rollback never revives any database, provider, or runtime-host credential revoked by the forward
fence. After the eligible database and deployment restore, the secret manager mints a distinct
rollback database-writer role credential, provider-writer credential set, and runtime-host authority
epoch and binds all three to the restored deployment. Every restored host must explicitly
re-register under the rollback epoch before it can heartbeat or claim work. The listener, intake,
and provider dispatch remain closed until the restored runtime proves database writes under only
the rollback role, provider access under only the rollback credential set, and rejection of the
revoked pre-cutover and abandoned vNext credentials and epochs. A rollback credential is never
shared with or made usable by either fenced deployment.

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
| Ordinary runtime job or agent process survives the fence | Critical | Fence every command/job claim, drain or cancel claimed work, reconcile completion candidates, terminate process groups, and verify zero workspace leases/processes before provider or database revocation |
| Old provider writer mutates remote state after archive | Critical | Cancel only proven-undispatched actions; reconcile every possible dispatch; revoke credentials; verify zero provider writers before archive |
| Old runtime host is restored or resumes heartbeats | Critical | Archive then reset only the configured runtime-state snapshot, rotate the host authority epoch, reject old-epoch registration/heartbeat, and verify empty vNext host/cache state |
| Shared task rows are over-deleted | Critical | Fixed predicates, locked counts, cleanup preconditions, preservation tests |
| Old provider object re-enters as new work | High | Provider intake fences and fail-closed unverifiable bindings |
| Archive becomes an accidental runtime dependency | High | No vNext archive reader; offline tooling and isolated restore only |
| Rollback crosses incompatible credentials/data or remote side effects | Critical | Permit restore only before any vNext provider dispatch; mint rollback-only database/provider/host credentials and bind them to the restored deployment; otherwise quiesce and recover forward |

## 10. Approval Gates

This RFC cannot move to `Approved` until all of the following exist:

- owner approval of the retention and downtime policy;
- attached real-database dry-run results meeting the feasibility targets;
- an exact reviewed manifest version;
- a versioned archive format and successful restore drill;
- a tested old-role/session fencing procedure;
- a tested all-job claim fence and runtime drain proving zero claimed jobs, live agent processes,
  completion candidates awaiting reconciliation, and runtime-owned workspace leases;
- a tested provider-writer drain, credential revocation, and zero-writer verification procedure;
- provider-fence conformance tests for poll, webhook, and direct submission;
- a rollback rehearsal that proves intake remains closed before activation, pre-activation restore,
  issuance and restored-deployment binding of distinct rollback database-writer, provider-writer,
  and host-authority credentials, rejection of every superseded credential and epoch, and
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
