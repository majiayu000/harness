# Tech Spec

## Linked Issue

GH-1602

## Product Spec

`specs/GH1602/product.md`

## Codebase Context (verified at `edc63a13`)

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Runtime-host defaults | `crates/harness-server/src/runtime_hosts.rs:5-6` | Heartbeat timeout and default lease are both 60 seconds. | Renewal cadence and duration bounds need one server-owned contract. |
| Host lifecycle | `crates/harness-server/src/runtime_hosts.rs:8-103` | A host has registration/heartbeat metadata and derived online state, but no draining state. | Deregistration needs a durable, retryable intermediate state. |
| Deregistration | `crates/harness-server/src/handlers/runtime_hosts.rs:95-150` | Releases legacy task claims, removes host/cache state, and persists the registry; workflow runtime jobs are untouched. | B-009/B-010 require workflow-job revocation before removal. |
| Claim handler | `crates/harness-server/src/handlers/runtime_hosts.rs:245-382` | Claims only `RuntimeKind::RemoteHost`, then records `RuntimeJobClaimed` best-effort in a separate transaction. | Claim response needs additive generation evidence; audit must join the claim transaction. |
| Completion handler | `crates/harness-server/src/handlers/runtime_hosts.rs:385-483` | Commits only for matching host and exact expiry; stale ownership returns 409. | New clients should add generation evidence while legacy request fields remain valid. |
| Lease parsing | `crates/harness-server/src/handlers/runtime_hosts.rs:519-557` | Missing duration defaults to 60; zero is accepted; very large values fail only if timestamp construction overflows. | B-005/B-006 require shared explicit bounds and no silent acceptance of zero. |
| Routes and auth | `crates/harness-server/src/http/http_router.rs:108-135`, `:198-201` | Claim/complete/deregister routes exist; no renew route; router is wrapped by `api_auth_middleware`. | Add one route without bypassing the existing auth layer (B-012). |
| Runtime job model | `crates/harness-workflow/src/runtime/model.rs:526-607` | Job JSON contains optional lease and defaulted `lease_generation`; claim increments generation and renewal preserves it. | Preserve legacy JSON compatibility while adding renewal evidence outside the job document. |
| Runtime store migrations | `crates/harness-workflow/src/runtime/store_migrations.rs:64-100` | Runtime jobs and ordered runtime events have durable tables, but renewal receipts do not. | Add a receipt table with explicit current-generation retention and cleanup. |
| Claim/reclaim store path | `crates/harness-workflow/src/runtime/store.rs:1028-1086` | Pending or expired-running jobs are row-locked and claimed; no audit event is inserted in that transaction. | B-001/B-004/B-011/B-015 need atomic claim/reclaim evidence. |
| Runtime event store | `crates/harness-workflow/src/runtime/store.rs:1089-1121` | Event sequence allocation and insertion are transactional but normally separate from job mutation. | Factor a transaction-local event append helper for atomic lease evidence. |
| Completion store path | `crates/harness-workflow/src/runtime/store.rs:1287-1468` | Job, command, workflow event, decision, follow-up commands, and instance state commit atomically after lease validation. | Keep this boundary; extend optional generation validation without splitting completion. |
| Existing renewal primitive | `crates/harness-workflow/src/runtime/store.rs:1471-1513` | Row-locks and extends when status, owner, and exact current expiry match; it does not check generation, post-expiry renewal, duration bounds, idempotency, or audit. | Replace or wrap with a complete fenced renewal transaction. |
| Local renewal | `crates/harness-workflow/src/runtime/worker.rs:230-280` | Local workers renew periodically and stop execution after loss. | Remote protocol should match its ownership-loss safety without changing local scheduling. |
| API integration tests | `crates/harness-server/src/handlers/runtime_hosts_workflow_api_tests.rs:128-400` | Covers RemoteHost-only claim, duplicate claim, expiry reclaim, completion, and missing job; DB tests skip without configured PostgreSQL. | Extend with deterministic renewal/deregistration cases. |
| Existing protocol doc | `docs/runtime-host-model-spec.md:59-109` | Documents claim/complete fencing by exact expiry and says deregistration releases pending task claims. | Update protocol, client obligations, and revocation semantics. |

## Proposed Design

### 1. Wire contract

Add:

```text
POST /api/runtime-hosts/{host_id}/runtime-jobs/{runtime_job_id}/lease/renew
```

Request body:

```json
{
  "lease_generation": 3,
  "lease_expires_at": "2026-07-15T10:01:00Z",
  "renewal_id": "client-generated-uuid",
  "lease_secs": 60
}
```

`lease_secs` uses the documented default when omitted. Centralize validation
for claim and renewal: accepted range `1..=3600` seconds, default 60. It is a
target TTL measured from the server's transaction time, not an extension added
to the remaining lease. Claim computes `now + lease_secs`; renewal computes
`next_lease_expires_at = max(current_lease_expires_at, now + lease_secs)`.
Consequently every accepted value has defined behavior, renewal never shortens
an existing lease, and the result is never later than the configured
`now + 3600 seconds` horizon. A persisted current expiry already beyond that
horizon is an invariant violation and renewal returns non-success rather than
clamping or extending it. If the current lease already meets a shorter target,
the request succeeds with the unchanged current expiry and one receipt/event;
it is not rejected merely because the requested target is shorter than the
remaining lease. Client clocks never choose the new expiry.

Success (`200`):

```json
{
  "renewed": true,
  "runtime_job_id": "...",
  "lease_generation": 3,
  "lease_expires_at": "2026-07-15T10:02:00Z",
  "replayed": false
}
```

An idempotent replay returns the same payload with `replayed: true`. Invalid
shape/duration is `400`; unknown job/host is `404`; stale, expired, wrong, or
draining ownership is `409` with stable `error_code: "lease_lost"` and
`must_stop: true`; store unavailability is `503`. Responses never expose the
current owner's ID or lease token.

Claim responses add top-level `lease_generation` while retaining the embedded
`runtime_job`. `CompleteRuntimeJobRequest` gains optional
`lease_generation`; new clients send it and the store enforces it. Absence
retains the current exact-owner-and-expiry V1 behavior for compatibility.

### 2. Additive renewal-receipt persistence and idempotency

Add a `runtime_job_lease_renewal_receipts` table keyed by runtime job, lease
generation, and renewal ID. Each row stores:

```text
runtime_job_id, renewal_id, owner, lease_generation, previous_expires_at,
renewed_expires_at, lease_secs, created_at
```

Retain every receipt whose successful `renewed_expires_at` is still in the
future for the current live generation; retaining only the latest receipt
would make an older lost response unsafe to retry. A matching receipt is
replayable only while the job is still `running` as `RemoteHost`, the same
active owner and generation remain current, and that receipt's successful
expiry is later than server `now`. B-007 idempotency is therefore available
for the lifetime of the successful result; once that expiry passes, B-003
takes precedence and the request cannot return success.

Delete expired receipt rows opportunistically in the renewal transaction.
Delete all receipts for the prior generation when a job is reclaimed, and all
receipts for the current generation when the job completes, is cancelled, or
is revoked. Those ownership-ending transitions and receipt cleanup share the
same transaction. Durable runtime events remain after receipt cleanup as the
audit history. Existing runtime-job JSON needs no migration and no receipt is
fabricated for an older job.

### 3. Atomic fenced renewal

Add a store result enum such as `Renewed`, `Replayed`, `LeaseLost`, and
`NotFound`, and a method that accepts an explicit `now` from the handler/test.
Within one PostgreSQL transaction:

1. Select the runtime job `FOR UPDATE`.
2. Delete receipts whose successful expiry is no longer live. A terminal or
   cancelled job also clears every receipt for its current generation.
3. Before consulting a receipt, require `running`, `RemoteHost`, the matching
   active owner and generation, and `current_expiry > now`. Expired, terminal,
   cancelled, revoked, reclaimed, wrong-owner, and wrong-generation requests
   therefore cannot replay a success.
4. Look up this generation and `renewal_id`. If every
   stored input matches and `renewed_expires_at > now`, return its exact prior
   result without another mutation or event. If the ID exists with different
   inputs, return a conflict.
5. When no live receipt matches, require the request's prior expiry to equal
   the job's exact current expiry; otherwise return `LeaseLost`.
6. Validate the target-TTL calculation and reject a pre-existing expiry beyond
   the configured horizon rather than shortening or clamping it.
7. Persist the renewed lease and insert the renewal receipt without changing
   generation.
8. Append one `RuntimeJobLeaseRenewed` event through a transaction-local event
   helper, including generation, previous/new expiry, renewal ID, and source.
9. Commit once and return the persisted expiry.

A known-job fenced rejection appends one
`RuntimeJobLeaseRenewalRejected` event with a closed reason class (for example
`expired`, `wrong_owner`, `wrong_generation`, `stale_expiry`, `wrong_state`,
`host_draining`) without recording another owner's values. Malformed,
unauthenticated, and unknown-job requests are handled before a job event can be
attributed. Rejection event insertion and the no-mutation decision share the
row-lock transaction.

### 4. Atomic claim/reclaim evidence

Refactor the RemoteHost-specific claim store path so the job mutation and
`RuntimeJobClaimed` or `RuntimeJobReclaimed` event use one transaction. The
event records owner, generation, expiry, and claim API. Remove the handler's
best-effort post-claim event write. Local-worker behavior stays unchanged
unless it opts into the same helper in a later change.

### 5. Draining and deregistration revocation

Add an additive host lifecycle value `active | draining`, defaulting legacy
records to `active`. Claim and renew handlers require `active`; heartbeat may
refresh a draining host but cannot reactivate it. Deregistration becomes a
retryable sequence:

1. Mark the host `draining` and persist runtime-host state. If persistence
   fails, restore/retain the prior state and return non-success.
2. In one workflow-store transaction, row-lock every `running` RemoteHost job
   owned by the host, clear its lease and all current-generation renewal
   receipts, move it to `pending`, and append one `RuntimeJobLeaseRevoked`
   event per job. Keep generation until the next claim increments it.
3. Release legacy pending-task claims as today and clear the project cache.
4. Remove the host and persist registry state. If a later step fails, keep the
   host draining; a retry re-runs revocation/release idempotently.

The handler reports success only after no workflow runtime job remains leased
to the host and host removal is durable. Late renewals and completions fail
because status/lease fencing no longer matches.

### 6. Remote executor protocol

Document renewal at approximately one third of TTL with at most one renewal
request in flight per job. A timeout is retried with the same `renewal_id` and
input. `409`, `404`, `must_stop: true`, or inability to confirm renewal before
the last confirmed expiry requires cancellation of local execution and
suppression of completion. Heartbeat is host liveness only and never changes
job leases.

## Data Flow

1. Authenticated active host calls claim; the store locks a pending/expired
   RemoteHost job, increments generation, writes lease plus claim/reclaim event,
   and returns job, generation, and expiry.
2. Host executes and submits fenced renewal evidence plus `renewal_id`.
3. Handler validates input bounds and registration lifecycle, captures server
   `now`, and calls the single store renewal transaction.
4. Store returns renewed/replayed/lost/not-found; handler maps it to the stable
   HTTP contract without exposing competing ownership.
5. Completion carries the latest expiry and, for upgraded clients, generation;
   the existing atomic completion path accepts only current ownership.
6. Deregistration marks draining, revokes workflow leases with events, releases
   legacy claims, and durably removes the host.

## Alternatives Considered

- **Long fixed leases:** rejected because they delay recovery after host loss
  and still cannot safely cover unbounded agent work.
- **Heartbeat renews every job:** rejected because host liveness does not prove
  progress or ownership of each job and would retain abandoned work.
- **Owner plus expiry only:** insufficient for same-host reclaim and weaker
  than the existing `lease_generation` model.
- **Client-provided absolute next expiry:** rejected because clock skew and an
  untrusted horizon could shorten or monopolize a lease.
- **Non-idempotent renewal:** rejected because a lost success response creates
  an unavoidable stop-or-double-extension ambiguity.
- **Delete the host and wait for expiry:** rejected because deregistration
  would report success while stale jobs remain non-reclaimable.

## Product-to-Test Mapping

| Invariant | Implementation area | Deterministic verification |
| --- | --- | --- |
| B-001 | store renewal + RemoteHost claim | fixed `t0`: renew A, assert B cannot claim at original expiry and can after renewed expiry |
| B-002 | renew handler/store fence tuple | table test for status, runtime kind, active registration, owner, generation, and exact expiry; only full match returns `Renewed` |
| B-003 | renewal result mapping + receipt precedence | seed matching receipts for expired and terminal jobs and assert non-success/no extension; table-test cancelled/revoked/reclaimed/wrong-owner/wrong-generation/stale-expiry cases and assert unchanged lease state |
| B-004 | `RuntimeJob::claim` and completion fence | renew preserves generation; same-host reclaim increments it; old-generation renew and completion return conflict |
| B-005 | shared duration parser | handler tests for missing default, zero, `3601`, `u64::MAX`, malformed/null; table-test every boundary value (`1`, default `60`, `3600`) as a target TTL with a coherent result |
| B-006 | server-time target-TTL calculation | fixed-now tests assert `max(current, now + lease_secs)`, a shorter target succeeds without shortening, boundary `3600` succeeds at exactly the horizon, and a nonconforming pre-existing over-horizon expiry is rejected without clamping |
| B-007 | current-generation renewal receipt table | replay identical input before its result expiry and assert the original expiry/one event; vary each input and assert conflict/no extension; advance past receipt expiry and assert B-003 non-success plus cleanup |
| B-008 | HTTP ownership-loss contract + protocol docs | integration test asserts `409`, `lease_lost`, `must_stop`, no accepted stale completion; docs review verifies ambiguous-timeout rule |
| B-009 | host lifecycle + revocation store method | deregister host with two leased jobs; assert draining blocks claim/renew, jobs become pending, host removed only after revocation |
| B-010 | idempotent deregistration | failpoint after draining and after revocation; retry and assert one revocation event per job and eventual removal |
| B-011 | transaction-local runtime events | rollback/failpoint tests prove no claim/renew/revoke mutation without event and no success event without mutation; known rejection has one ordered event |
| B-012 | router auth and sanitized errors | router test rejects unauthenticated renew; wrong host receives no competing owner/expiry fields |
| B-013 | heartbeat and per-job isolation | heartbeat plus renewal of job A leaves job B expiry byte-identical |
| B-014 | serde/wire compatibility | deserialize legacy job JSON; existing claim/complete fixtures remain valid; new claim response fields are additive |
| B-015 | row-lock race tests | barrier-controlled concurrent renew/reclaim, renew/complete, and renew/deregister tests assert exactly one winner and one mutation |

## Risks

- **Security:** The existing shared API authentication does not provide
  cryptographic per-host identity. Exact lease evidence is a fencing token,
  not a replacement for auth. Responses and audit rejections must not leak the
  winning host's lease details.
- **Compatibility:** Tightening duration validation rejects zero and
  over-maximum claims that were previously accepted. These are unsafe inputs;
  document the range and target-TTL meaning, and retain all valid existing
  request fields. Completion generation is additive and optional during
  rollout.
- **Crash consistency:** Host registry persistence and PostgreSQL job
  revocation cannot share one transaction. Persisted `draining` is the durable
  recovery state; retries must converge before success is reported.
- **Performance:** Deregistration may lock many owned jobs. Select by running
  RemoteHost lease owner, process in a bounded transaction, and add an index or
  generated lease-owner column only if query-plan evidence requires it.
- **Audit volume:** Rejection events can be noisy. Record only authenticated,
  syntactically valid requests for known jobs, use closed reason codes, and do
  not silently drop accepted mutation evidence.
- **Maintenance:** Claim, renew, complete, cancel, and revoke must share
  lease-fence and receipt-cleanup helpers so their
  status/owner/generation/expiry semantics cannot drift.
- **External side effects:** A disconnected stale process may ignore the stop
  contract. Generation fencing protects Harness state but cannot revoke
  already-issued third-party effects; adapters still need idempotency keys.

## Verification Plan

- `cargo test -p harness-workflow runtime_store_remote_host_lease`
- `cargo test -p harness-server runtime_job_lease_renewal`
- `cargo test -p harness-server runtime_host_deregister`
- `cargo test -p harness-server runtime_hosts_workflow_api`
- `cargo check --workspace --all-targets`
- `cargo fmt --all -- --check`
- Before PR push: `cargo clippy --workspace --all-targets -- -D warnings`
- PostgreSQL tests use fixed timestamps and barriers/failpoints, never sleeps;
  they must run with `HARNESS_DATABASE_URL` rather than silently supplying
  evidence from skipped DB tests.

## Rollback Plan

Stop RemoteHost clients from calling renewal, then remove/disable the renewal
route and restore claim duration parsing if necessary. The additive host
lifecycle remains serde-compatible, and the unused receipt table can remain
until a later migration removes it. Before removing draining/revocation
behavior, verify no host is `draining` and no workflow job depends on a renewed
expiry. Existing claim/completion behavior then resumes, with the original
finite lease safety limitations documented as the rollback consequence.
