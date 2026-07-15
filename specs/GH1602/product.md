# Product Spec

## Linked Issue

GH-1602

## User Problem

External runtime hosts can claim and complete workflow runtime jobs, but they
cannot keep a valid lease while an agent turn is still running. The default
lease lasts 60 seconds, while repository work, provider waits, and verification
often take longer. After expiry, another host may reclaim the job even though
the first host is still producing repository or GitHub side effects. The final
completion fence prevents two terminal database commits, but it cannot undo
side effects emitted by both executors.

Operators also expect deregistering a runtime host to surrender all of that
host's work. Today deregistration releases legacy task claims but leaves
workflow runtime-job leases active until they expire.

## Goals

1. Give each active RemoteHost job a bounded renewal protocol that preserves
   exclusive ownership for long-running execution.
2. Make every renewal fenced against stale, expired, reclaimed, wrong-host,
   and terminal job state.
3. Make renewal retries idempotent so a lost HTTP response cannot extend the
   lease twice or force unsafe guesswork.
4. Give remote executors an explicit stop contract when ownership is lost or
   cannot be confirmed before expiry.
5. Drain and revoke workflow runtime-job leases before a host is deregistered.
6. Preserve durable, ordered evidence for claims, renewals, rejections,
   revocations, and reclaims.
7. Preserve existing valid claim and completion clients through additive API
   fields and unchanged behavior for clients that do not opt into renewal.

## Non-Goals

- Undoing repository, GitHub, or provider side effects already emitted by a
  stale executor.
- Replacing the existing API authentication layer with per-host credentials.
- Adding progress streaming or remotely killing an operating-system process.
- Changing local runtime-worker scheduling or lease-renewal behavior.
- Changing the legacy pending-task lease protocol.
- Guaranteeing exactly-once external side effects; the protocol prevents
  concurrent lease ownership and provides fencing evidence.

## User-Visible Behavior

A registered active host claims a RemoteHost workflow job and receives its
lease generation and expiry. While executing, it periodically sends a renewal
request containing the exact lease evidence from its last successful claim or
renewal and a unique renewal request ID. A successful response returns the new
server-computed expiry. A safe retry of the same request returns the same
result.

If renewal is rejected because ownership is stale, expired, revoked, or
otherwise invalid, the response identifies ownership loss without revealing a
different owner's identity and directs the host to stop. If the host cannot
resolve an ambiguous network result before its last confirmed expiry, it must
also stop and must not submit completion.

Deregistration moves the host into a draining state first. Draining hosts
cannot claim or renew work. Their workflow runtime-job leases become
reclaimable and stale completion is rejected before the host disappears from
the registry. A partial deregistration remains visibly draining and can be
retried; it never reports success while leases remain owned by the host.

## Testable Invariants

- B-001: A successfully renewed, unexpired RemoteHost lease prevents every
  other host from claiming the same runtime job until the renewed expiry.
- B-002: Renewal succeeds only when the target job exists, is `running`, has
  `runtime_kind = remote_host`, and the registered active host, lease
  generation, and exact current expiry all match the request.
- B-003: A request against an expired, terminal, cancelled, revoked, reclaimed,
  wrong-host, wrong-generation, or stale-expiry lease performs no lease
  extension and returns a non-success ownership result.
- B-004: Lease generation remains unchanged across renewals and increments on
  every new claim after expiry or revocation, including reclaim by the same
  host ID; evidence from an older generation can never renew or complete the
  newer claim.
- B-005: The server computes the new expiry. A requested lease duration must be
  present or use the documented default, must be positive, and must not exceed
  the documented maximum; invalid, empty, or overflow-inducing input is
  rejected rather than clamped or treated as success.
- B-006: A renewal never shortens the currently confirmed lease and never
  extends it beyond one configured maximum-duration window from server time.
- B-007: Repeating the same renewal request ID with the same host, generation,
  prior expiry, and duration returns the original successful expiry and emits
  no second renewal effect or audit event; reusing that ID with different
  inputs is rejected.
- B-008: A host receiving an ownership-loss response must stop the affected
  executor and suppress completion. After an ambiguous transport failure it
  may retry the same request ID, but if ownership is not confirmed before the
  last confirmed expiry it must stop.
- B-009: A successful deregistration first makes the host draining, blocks its
  new claims and renewals, revokes all of its running RemoteHost job leases to
  a reclaimable state, and only then removes it from the host registry.
- B-010: If deregistration fails after draining begins, the host remains
  visibly draining, cannot regain work, and a repeated deregistration resumes
  cleanup idempotently without duplicating revocation effects.
- B-011: For a known runtime job, each successful claim/reclaim, successful
  renewal, rejected syntactically valid renewal, and deregistration revocation
  has durable ordered audit evidence; the state mutation and its success or
  revocation evidence cannot be committed separately.
- B-012: All runtime-host routes remain protected by the existing API
  authentication. Host ID alone is not lease authority, and an ownership-loss
  response does not disclose another host's identity or lease evidence.
- B-013: Heartbeat freshness and job-lease renewal remain separate: heartbeat
  does not extend any job lease, and renewing one job does not renew another.
- B-014: Existing claim and completion request/response fields retain their
  meaning. Claim adds lease generation as additive evidence, and a valid client
  that never calls renewal continues to run until its originally claimed
  expiry under the current completion contract.
- B-015: Concurrent renew, reclaim, completion, and deregistration operations
  resolve through one durable lease order: at most one operation wins for a
  generation, losers observe a non-success result, and no loser mutates job or
  workflow state.

## Acceptance Criteria

- [ ] Remote hosts can renew a claimed workflow runtime-job lease through the
  authenticated API using exact fenced lease evidence.
- [ ] Lease duration is bounded and renewal retries are idempotent.
- [ ] Ownership-loss responses and documentation define the mandatory remote
  executor stop behavior.
- [ ] Host deregistration drains and revokes workflow runtime-job leases before
  removal and remains retry-safe after partial failure.
- [ ] Claim, renewal, rejection, revocation, and reclaim evidence is durable
  and ordered with the corresponding lease operation.
- [ ] Existing claim and completion API fields remain compatible and all new
  fields are additive.
- [ ] Deterministic PostgreSQL tests cover every B-001 through B-015 invariant,
  including races and same-host generation reuse.

## Boundary Checklist

| Category | Coverage |
| --- | --- |
| Empty / missing input | covered: B-005 rejects absent-invalid, zero, oversized, and overflow-inducing duration/input without silent clamping |
| Error and failure paths | covered: B-003 ownership/state failures, B-008 ambiguous transport failure, B-010 partial deregistration |
| Authorization / permission | covered: B-002 requires registered active owner plus exact lease evidence; B-012 preserves API auth and prevents owner disclosure |
| Concurrency / race / ordering | covered: B-004 generation fencing and B-015 deterministic single-winner ordering |
| Retry / repetition / idempotency | covered: B-007 renewal replay and B-010 deregistration replay |
| Illegal state transitions | covered: B-002 allows renewal only from matching `running` RemoteHost state; B-003 rejects terminal/revoked/reclaimed state |
| Compatibility / migration | covered: B-014 additive wire behavior; renewal metadata must tolerate older persisted jobs without fabrication |
| Degradation / fallback | covered: B-008 requires stop rather than treating uncertain ownership as success; B-010 keeps partial deregistration visibly draining |
| Evidence and audit integrity | covered: B-011 requires ordered evidence in the same commit as successful lease mutation/revocation |
| Cancellation / interruption / partial completion | covered: B-008 suppresses stale completion and B-009/B-010 define interrupted deregistration behavior |

## Edge Cases

- The same host ID reclaims an expired job: the new generation fences all old
  renewal and completion messages even though the owner string is unchanged.
- The renewal response is lost after the server commits: replaying the same
  request ID returns the committed expiry without extending again.
- Renewal and reclaim arrive at the expiry boundary: server time and the
  transactional job lock select one winner; expiry is not client-clock based.
- Deregistration races with renewal or completion: draining/revocation and the
  lease transaction select one winner, and the loser cannot mutate state.
- The workflow runtime store is unavailable: renewal/deregistration does not
  report success or fall back to an in-memory lease.
- A legacy persisted job has no renewal receipt: it remains readable and can
  use the existing claim/completion behavior, but no idempotent renewal result
  is fabricated.

## Rollout Notes

Ship the API and additive persistence fields before remote clients begin
renewing. Update remote clients to send renewal IDs and generations and to
honor the stop contract, then enable periodic renewal. Existing clients remain
bounded by their original lease. Rollback disables/removes the renewal route;
additive persisted renewal evidence remains harmless and readable.
