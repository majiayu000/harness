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
| Runtime job model | `crates/harness-workflow/src/runtime/model.rs:526-607` | Job JSON contains optional lease and defaulted `lease_generation`; claim increments generation and renewal preserves it. | Add optional renewal receipt; preserve legacy JSON compatibility. |
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
for claim and renewal: accepted range `1..=3600` seconds, default 60. The
server computes `next_lease_expires_at = max(current_lease_expires_at, now) +
lease_secs`, then rejects any result beyond `now + 3600 seconds`. Client clocks
never choose the new expiry.

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

### 2. Additive job persistence and idempotency

Add a serde-defaulted, skip-when-absent `last_lease_renewal` object to
`RuntimeJob`:

```text
renewal_id, owner, lease_generation, previous_expires_at,
renewed_expires_at, lease_secs
```

Only the latest receipt is retained. `RuntimeJob::claim` clears the receipt
before incrementing generation. A renewal retry is a replay only when every
stored receipt input matches. A reused `renewal_id` with different input is a
conflict. Older job JSON deserializes with `None`; no migration or fabricated
receipt is required.

### 3. Atomic fenced renewal

Add a store result enum such as `Renewed`, `Replayed`, `LeaseLost`, and
`NotFound`, and a method that accepts an explicit `now` from the handler/test.
Within one PostgreSQL transaction:

1. Select the runtime job `FOR UPDATE`.
2. Check a matching replay receipt first and return its exact prior result.
3. Require `running`, `RemoteHost`, matching owner, generation, exact current
   expiry, and `current_expiry > now`.
4. Validate the server-computed next expiry against duration/horizon bounds.
5. Persist the renewed lease and renewal receipt without changing generation.
6. Append one `RuntimeJobLeaseRenewed` event through a transaction-local event
   helper, including generation, previous/new expiry, renewal ID, and source.
7. Commit once and return the persisted expiry.

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
   owned by the host, clear its lease and renewal receipt, move it to `pending`,
   and append one `RuntimeJobLeaseRevoked` event per job. Keep generation until
   the next claim increments it.
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
| B-003 | renewal result mapping | table test for expired/terminal/cancelled/revoked/reclaimed/wrong-owner/wrong-generation/stale-expiry; assert unchanged job JSON |
| B-004 | `RuntimeJob::claim` and completion fence | renew preserves generation; same-host reclaim increments it; old-generation renew and completion return conflict |
| B-005 | shared duration parser | handler tests for missing default, zero, `3601`, `u64::MAX`, malformed/null; only documented range succeeds |
| B-006 | server-time expiry calculation | fixed-now unit tests prove no shortening and no expiry beyond `now + 3600s` |
| B-007 | renewal receipt replay | repeat identical renewal ID and assert same expiry/one event; vary each input and assert conflict/no extension |
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
  document the range and retain all valid existing request fields. Completion
  generation is additive and optional during rollout.
- **Crash consistency:** Host registry persistence and PostgreSQL job
  revocation cannot share one transaction. Persisted `draining` is the durable
  recovery state; retries must converge before success is reported.
- **Performance:** Deregistration may lock many owned jobs. Select by running
  RemoteHost lease owner, process in a bounded transaction, and add an index or
  generated lease-owner column only if query-plan evidence requires it.
- **Audit volume:** Rejection events can be noisy. Record only authenticated,
  syntactically valid requests for known jobs, use closed reason codes, and do
  not silently drop accepted mutation evidence.
- **Maintenance:** Claim, renew, complete, and revoke must share lease-fence
  helpers so their status/owner/generation/expiry semantics cannot drift.
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
lifecycle and renewal receipt fields remain serde-compatible and can be left
in persisted JSON. Before removing draining/revocation behavior, verify no host
is `draining` and no workflow job depends on a renewed expiry. Existing
claim/completion behavior then resumes, with the original finite lease safety
limitations documented as the rollback consequence.
