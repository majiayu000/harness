# Product Spec

## Linked Issue

GH-1767

## User Problem

Cross-agent review is the harness's independence guarantee between
implementation and merge. Today that guarantee fails open: a challenger reply
that violates the tag protocol (empty output, refusal, quota error, untagged
prose) yields the verdict `APPROVED`, and a run with no challenger — or with
the same model in both roles — produces a result indistinguishable from a
genuine two-model review. Separately, the external-review escalation ladder
(bot quota / bot silence fallback) was deleted with the legacy `task_executor`
path (#1725) without a runtime replacement, leaving its data model
(`ReviewFallbackSnapshot` and tiers) producer-less and leaving workflows with
no defined behavior when the external reviewer never responds.

Operators need review verdicts that cannot be manufactured by protocol
violations, degraded runs that identify themselves, and a defined, escalating
response to external-reviewer unavailability.

## Goals

- Cross-review fails closed on challenger protocol violations.
- Every cross-review result names its mode and the identities of both
  reviewers; single-model and same-model runs are explicitly marked degraded.
- Define a runtime-owned external-review escalation ladder driven by
  server-collected GitHub snapshots, reusing the existing
  `ReviewFallbackSnapshot` tier model.
- End the producer-less state of the review-fallback data model: it gains a
  live runtime producer under this contract.

## Non-Goals

- Changing the server merge-gate policy (`auto_merge.rs` predicates) itself.
- Adding new external review-bot integrations or changing bot identities.
- Reintroducing the deleted `task_executor` code or its prose-parsing
  triggers.
- Changing the `pr_feedback` inspection ownership (it remains server-owned).
- Altering the GH-1715 legacy lifecycle transition contract.
- Head-SHA-bound local-review verification. The runtime's local review gate
  currently accepts `LocalReviewPassed` without binding the approval to the
  reviewed commit (the legacy `approved_review_sha` re-verification was
  deleted with `task_executor`); restoring that binding is a real gap but is
  deferred to a follow-up spec — this contract covers cross-review protocol
  integrity and external-review escalation only.

## User-Visible Behavior

1. **B-001:** A challenger reply containing no protocol tag lines
   (`CONFIRMED:` / `FALSE-POSITIVE:` / `MISSED:`) is a protocol failure. The
   cross-review result records verdict `PROTOCOL_FAILURE` for that run; it is
   never reported as `APPROVED`.
2. **B-002:** A protocol-failure verdict identifies the failing round, the
   challenger identity, and a bounded excerpt of the offending reply for
   diagnosis.
3. **B-003:** Every cross-review result carries `mode`
   (`cross_model` | `single_model_degraded`), `primary_agent_id`, and
   `challenger_agent_id`.
4. **B-004:** A run with no available challenger completes only as
   `single_model_degraded`; its verdict namespace is distinct (e.g.
   `APPROVED_DEGRADED`), so no consumer can mistake it for a two-model
   approval.
5. **B-005:** A run whose resolved challenger identity equals the primary
   identity is treated exactly as "no challenger": it cannot produce a
   `cross_model` result.
6. **B-006:** External-review escalation is a runtime ladder over the
   `pr_feedback` workflow: Tier A (primary external bot) → Tier B (alternate
   external bot) → Tier C (harness-internal independent review + operator
   gate). Tier transitions occur only on defined triggers observed from
   server-collected snapshots: reviewer quota exhaustion or reviewer silence
   past a configured threshold.
7. **B-007:** Each tier transition persists a `ReviewFallbackSnapshot` (tier,
   trigger, active bot, activation time) as durable, operator-visible
   evidence, honoring the GH-1715 preservation semantics (first snapshot
   wins; a conflicting logical fallback is an error).
8. **B-008:** Tier C never self-satisfies the merge gate: completing a Tier-C
   internal review routes the workflow to an operator gate; the merge
   predicate `review_decision == APPROVED` is not simulated or bypassed.
9. **B-009:** Escalation triggers derive only from server-owned GitHub
   snapshot data (review events, bot comments, timestamps) — never from
   agent-authored activity results.
10. **B-010:** When escalation is disabled by configuration, reviewer
    unavailability leaves the workflow in its waiting state and emits a
    distinct operator-attention signal instead of stalling silently.

## Acceptance Criteria

- [ ] Unit tests prove empty, refusal-shaped, and untagged challenger replies
      produce `PROTOCOL_FAILURE`, never `APPROVED`, at every round position.
- [ ] Tests prove tagged replies still classify CONFIRMED / FALSE-POSITIVE /
      MISSED exactly as before (no behavior change for protocol-conforming
      output).
- [ ] Tests prove `mode`, `primary_agent_id`, and `challenger_agent_id` are
      populated in all paths, and that the no-challenger and same-identity
      paths produce `single_model_degraded` with the degraded verdict
      namespace.
- [ ] Runtime tests prove Tier A→B and B→C transitions fire only on the
      defined snapshot-derived triggers, persist exactly one fallback
      snapshot per logical fallback, and are idempotent under repeated
      observation.
- [ ] Tests prove Tier-C completion routes to an operator gate and cannot
      reach `merging` without it.
- [ ] Tests prove agent-authored activity results cannot trigger or advance
      escalation.
- [ ] `ReviewFallbackSnapshot` has at least one non-test producer, or —
      should the escalation scope be cut during implementation — the model
      and `record_ready_to_merge_with_fallback` are removed in the same
      change (no third producer-less state).

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001, B-004: empty challenger output and absent challenger are both explicit, named outcomes. |
| Error and failure paths | Covered by B-001, B-002, B-010: protocol failure and reviewer unavailability are visible errors/signals, never success-shaped. |
| Authorization / permission | Covered by B-008: Tier C cannot satisfy the merge approval predicate; operator authority is preserved. |
| Concurrency / race / ordering | Covered by B-007 via GH-1715 first-snapshot-wins semantics; repeated trigger observation is idempotent. |
| Retry / repetition / idempotency | Covered by B-007; re-observing the same trigger does not duplicate snapshots or re-escalate. |
| Illegal state transitions | Covered by B-006: tier order is monotonic A→B→C; skipping or reversing tiers is illegal. |
| Compatibility / migration | Covered by Non-Goals: merge gate, snapshot ownership, and GH-1715 contract unchanged; existing rows readable. |
| Degradation / fallback | Covered by B-003–B-005: degradation is always named, never silent. |
| Evidence and audit integrity | Covered by B-002, B-007, B-009: all escalation evidence is server-derived and durable. |
| Cancellation / interruption / partial completion | Covered by B-010 and B-007: interrupted escalation leaves a durable snapshot and a waiting state with operator signal. |

## Edge Cases

- Challenger returns tags plus a trailing quota-error line (valid round; the
  error text is not a tag and is ignored).
- Challenger returns only `FALSE-POSITIVE:` lines (valid: consensus empty,
  contested populated — genuine `APPROVED`, not protocol failure).
- The registry's default agent is the same binary/model as the named
  challenger under a different registration key.
- The external bot posts a quota-exhaustion comment and later recovers before
  the silence threshold elapses.
- Both external bots are quota-exhausted in the same sweep (A→B→C in rapid
  succession must still record each transition).
- Escalation configured off while a persisted Tier-B snapshot exists from an
  earlier deployment.

## Rollout Notes

Protocol-failure hardening (B-001–B-005) is a behavior change for consumers
that previously received `APPROVED` from malformed challenger output; monitor
cross-review failure rates after rollout — a spike reveals latent challenger
misconfiguration, not a regression. The escalation ladder ships behind
configuration (default off for one release), then defaults on. Reverting
restores fail-open review verdicts and producer-less fallback state.
