# Product Spec

## Linked Issue

GH-1704

## User Problem

Exact workflow replay can depend on a raw agent transcript that currently has
weaker durability than the workflow that references it. If that transcript is
missing, unreadable, or corrupt, the replay consumer blocks without a valid
recovery action and may still appear as active work. Operators need transcript
evidence to remain verifiable for the complete dependency lifetime and need an
explicit terminal or reconstruction path when it cannot be read.

## Goals

- Persist replay-required transcript bytes before acknowledging their producer.
- Bind evidence to stable identity, producer ownership, byte length, and
  checksum.
- Keep required transcripts retained until all dependent workflows terminate.
- Validate transcript availability and integrity before exact-replay dispatch.
- Retry only transient storage failures with bounded policy.
- Classify confirmed evidence loss terminally with operator guidance.
- Support authenticated provider re-export reconstruction.

## Non-Goals

- Retaining every diagnostic log indefinitely.
- Replaying from summaries when exact raw evidence is required.
- Treating checksum mismatch or missing evidence as a warning-plus-fallback.
- Exposing transcript bodies through unauthenticated routes.
- Redesigning unrelated workflow artifacts or agent protocols.

## User-Visible Behavior

1. **B-001:** An activity result that advertises replayable transcript evidence
   is accepted only if transcript persistence and activity completion commit
   atomically.
2. **B-002:** Durable evidence records a stable artifact reference, producer
   runtime-job ID, exact byte length, and SHA-256 checksum; forged or
   mismatched ownership is rejected.
3. **B-003:** Transcript retention is pinned while any workflow in its
   dependency family is non-terminal and becomes GC-eligible only after the
   complete family is terminal.
4. **B-004:** Before exact replay starts, Harness verifies existence,
   readability, size, checksum, and ownership; the consumer is not dispatched
   on invalid evidence.
5. **B-005:** Transient store/read failures use the declared bounded retry and
   backoff policy without duplicating completion or replay work.
6. **B-006:** Confirmed missing or corrupt evidence becomes a terminal,
   explicitly classified failure with actionable operator guidance and is not
   projected as queued active work.
7. **B-007:** An authenticated reconstruction operation may import a provider
   re-export only when its workflow/job identity and checksum contract match;
   successful reconstruction makes a later preflight readable.
8. **B-008:** Local agent and `RemoteHost` completion paths produce the same
   durable transcript contract, including large transcripts.
9. **B-009:** Restart/replay is deterministic: committed evidence remains
   readable and an interrupted uncommitted transaction exposes neither a
   successful completion nor a partial transcript reference.
10. **B-010:** Transcript read/reconstruction APIs enforce existing server
    authentication, bounded bodies, and non-secret error reporting.

## Acceptance Criteria

- [ ] B-001 through B-010 map to executable verification.
- [ ] Atomic rollback and restart tests prove no completion/evidence split.
- [ ] Retention/GC tests cover parent-child dependency families.
- [ ] Missing, unreadable, wrong-size, checksum-mismatch, and forged-producer
      evidence all fail closed.
- [ ] Local and `RemoteHost` exact replay preflight behavior is equivalent.
- [ ] Authenticated reconstruction restores valid evidence and rejects invalid
      or oversized input.
- [ ] Exact-head CI, Gemini, independent review, thread audit, and SpecRail PR
      gate pass before implementation merge.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002, B-004, and B-007. |
| Error and failure paths | Covered by B-005 and B-006. |
| Authorization / permission | Covered by B-007 and B-010. |
| Concurrency / race / ordering | Covered by B-001 and B-009. |
| Retry / repetition / idempotency | Covered by B-005, B-007, and B-009. |
| Illegal state transitions | Covered by B-001, B-004, and B-006. |
| Compatibility / migration | Covered by B-008 and B-009; existing non-replay artifacts remain unchanged. |
| Degradation / fallback | Covered by B-004 and B-006; exact replay never degrades to summary replay. |
| Evidence and audit integrity | Covered by B-002, B-004, B-007, and B-010. |
| Cancellation / interruption / partial completion | Covered by B-001, B-003, and B-009. |

## Edge Cases

- Producer completion and transcript insert fail at different transaction
  boundaries.
- A valid reference is presented by the wrong runtime job or workflow.
- Transcript bytes change without changing the stored reference.
- A parent terminates while a dependent child still requires replay.
- A transient read error becomes a confirmed missing object after retries.
- Reconstruction is repeated after a prior successful import.
- Remote-host completion submits a transcript near the request-body limit.

## Rollout Notes

The database migration is additive and startup must establish the durable store
before accepting transcript-bearing completion. Existing workflows without a
durable transcript cannot be silently upgraded; exact replay for those rows
must fail with the classified recovery guidance. Revert application code only
after confirming no active workflow depends on the new reconstruction route.
